//! SSA-form intermediate representation (milestone 6a + 6b).
//!
//! Built on top of `crate::ir::TypedProgram` — the tree-shaped IR
//! is still authoritative; this module exposes a parallel SSA
//! layer that future analyses and (eventually) the backends will
//! consume. See [TODO.md](../TODO.md) for the migration plan.
//!
//! Design choices:
//!
//! - **Block arguments**, not explicit phi nodes (the Cranelift /
//!   Rust MIR convention). At a merge point, the joining block
//!   takes one parameter per binding that has different SSA
//!   names on its incoming edges; each predecessor's branch
//!   terminator passes the matching `ValueId` as an argument.
//!   Simpler to construct than phi-node SSA and uniform across
//!   all merge shapes (if/else, loop header, etc.).
//!
//! - **Functional dominator-free construction**. We snapshot the
//!   binding→ValueId map at the start of each branch, then
//!   diff at the merge to discover which bindings need a block
//!   parameter. No worklist, no iterated dominance frontier —
//!   sufficient for the language's current control-flow shapes
//!   (if/else and bounded for/while loops).
//!
//! - **Scalar subset for v1**. Vec/Array/IndexAssign, Strings,
//!   loops, parallel-for, reductions, refs, and tasks are
//!   deferred to later milestones. The lowerer returns an
//!   error when it encounters one of them so the surface fail
//!   loudly while the migration is in flight.

use std::collections::BTreeMap;
use std::fmt;

use crate::ast::{BinaryOp, Type, UnaryOp};
use crate::ir::{TypedExpr, TypedExprKind, TypedFunction, TypedProgram, TypedStmt};
use crate::span::Span;

/// SSA-named value. Sequentially assigned by [`FunctionBuilder`];
/// unique within a single function.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ValueId(pub u32);

/// SSA basic-block name. Sequentially assigned within a function.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub u32);

/// Compile-time constant operand. Kept separate from
/// [`crate::ir::TypedConst`] so this module doesn't pull on the
/// SMT layer's notion of folded constants.
#[derive(Clone, Debug, PartialEq)]
pub enum Const {
    Int(i128),
    Bool(bool),
    Float(f64),
}

impl fmt::Display for Const {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Const::Int(v) => write!(f, "{}", v),
            Const::Bool(true) => f.write_str("true"),
            Const::Bool(false) => f.write_str("false"),
            Const::Float(v) => write!(f, "{}", v),
        }
    }
}

/// Either a fresh SSA value or a compile-time constant. Lifted
/// out so an instruction's RHS can mix the two without forcing a
/// trivial `Const → ValueId` materialization for every literal.
#[derive(Clone, Debug, PartialEq)]
pub enum Operand {
    Value(ValueId),
    Const(Const),
}

impl fmt::Display for Operand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Operand::Value(v) => write!(f, "%{}", v.0),
            Operand::Const(c) => write!(f, "{}", c),
        }
    }
}

/// Body of a non-terminator instruction. Terminators live on
/// [`BasicBlock::terminator`] and aren't part of this enum.
#[derive(Clone, Debug, PartialEq)]
pub enum InstrKind {
    /// `%dst = const <c>` — materialize a literal as a named SSA
    /// value. The lowerer prefers to thread `Operand::Const`
    /// through directly; this form appears only when an
    /// expression's result must be named (e.g., a Var binding
    /// initialized to a literal).
    Const(Const),
    Unary { op: UnaryOp, x: Operand },
    Binary { op: BinaryOp, l: Operand, r: Operand },
    Call { name: String, args: Vec<Operand> },
    /// First-class function pointer to a top-level function.
    /// The IR keeps the source name so backends emit the
    /// matching symbol (`@fn_<name>` in LLVM, the bare
    /// declarator-prefixed identifier in C). The result type
    /// is `fn(T...) -> R`.
    FnRef { name: String },
    /// Indirect call through a fn-pointer value. `callee` is an
    /// SSA operand of fn-ptr type; the call applies it to
    /// `args`. Distinct from `Call` so passes that walk by
    /// name (lock-graph, purity audits) recognize the indirect
    /// shape without confusing it with a direct call.
    CallIndirect { callee: Operand, args: Vec<Operand> },
    /// Source-typed numeric cast. The lowerer doesn't yet split
    /// trunc / sext / zext / fpext etc. — that's a backend
    /// concern when the backends switch over.
    Cast { x: Operand, to: Type },
    /// String literal: emitted as an interned constant pointer
    /// at the backend boundary.
    StrLit(String),
    /// `[elt0, elt1, …, eltN]` array construction. The result
    /// has aggregate type `[N x T]`; backends materialize it
    /// into a stack alloca + element stores.
    ArrayLit { elements: Vec<Operand> },
    /// `array[index]` load. `array` is an SSA value of array,
    /// Vec, or Str type (the lowerer flattens through Refs at
    /// the operand source). `checked` mirrors the typed-IR
    /// field — true means the backend should emit a runtime
    /// bounds check, false means SMT has discharged it.
    Index { array: Operand, index: Operand, checked: bool },
    /// `len(x)` — `x: [T;N]` returns N, `x: Vec<T>` reads the
    /// struct's len field, `x: Str` calls strlen.
    Len { array: Operand, length: u64 },
    /// `&x` / `&mut x` — produces a reference (pointer) to the
    /// source value's storage. `source` is the SSA value the
    /// binding currently holds; the backend takes its address
    /// directly (alloca-backed in LLVM, declarator-backed in
    /// C). The lowerer fills in the operand by looking up the
    /// binding's current ValueId from its locals map.
    RefOf { source: Operand, mut_: bool },
    /// `array[index] = value` — in-place mutation. `array` is
    /// the SSA value of the array binding being indexed (an
    /// addressable local in C, an alloca pointer in LLVM).
    /// The instruction has no SSA result (returns ()-ish);
    /// for SSA-purity the result `ValueId` is still allocated
    /// but won't be referenced.
    IndexAssign { array: Operand, base_ty: Type, index: Operand, value: Operand, checked: bool },
    /// Affine drop — frees the storage owned by `source`.
    /// The lowerer translates `TypedStmt::Drop` to this; the
    /// `source` operand carries the SSA value being dropped
    /// so a future backend can hand off the precise address
    /// without re-discovering it from a name. `name` is kept
    /// for diagnostic / audit purposes (the drop-coverage
    /// audit reports it).
    Drop { source: Operand, name: String, ty: Type },
    /// Structural marker for parallel constructs. The body is
    /// lowered inline like the sequential equivalent (a regular
    /// for-loop, or a straight-line block for tasks); this
    /// instruction sits before the body's entry and lets
    /// backends / analyses recognize the shape without
    /// re-discovering it. Distinct from `Call` so passes that
    /// don't care can skip it cheaply.
    Hint(HintKind),
}

/// Subkinds for `InstrKind::Hint`. Each carries the metadata an
/// analysis or backend needs to recognize the parallel-region
/// shape without walking the surrounding code.
#[derive(Clone, Debug, PartialEq)]
pub enum HintKind {
    /// Sits at the entry of a `parallel for` lowering. The list
    /// names every binding that's combined across iterations
    /// (the source `reduce <var> with <op>;` clauses) so a
    /// reduction-aware backend can emit the right runtime
    /// reduction call without re-deriving the shape.
    ///
    /// `shape` records the structured-loop metadata the
    /// lowerer already knows but the block-level form would
    /// otherwise discard: the loop's counter (a phi-style
    /// block param on the header), its source-level name,
    /// the start/end operands, and the three loop blocks
    /// (`header`/`body`/`exit`). A backend can consume this
    /// directly to emit a structured `for (start; cond;
    /// step) { body }` with an OpenMP / libgomp parallel
    /// pragma, skipping the CFG-pattern-recognition step.
    ParallelForBegin {
        reductions: Vec<(String, crate::ast::ReductionOp, Type)>,
        shape: ParallelForShape,
    },
    /// Closes a `ParallelForBegin` region.
    ParallelForEnd,
    /// Opens a `task <name> { … }` region. Body runs inline.
    TaskBegin { handle: String },
    /// Closes a `TaskBegin` region.
    TaskEnd { handle: String },
    /// Marks a `join <name>;` — handle is consumed.
    TaskJoin { handle: String },
}

/// Structured-loop metadata captured by the SSA lowerer
/// for parallel-for regions. The block-level SSA form
/// already encodes the loop's CFG (header / body / exit
/// blocks with a phi-style counter on the header), but
/// recovering "this is a structured for-loop with these
/// bounds" requires a CFG-pattern recognizer. Keeping the
/// shape on the begin-hint lets backends consume it
/// directly.
#[derive(Clone, Debug, PartialEq)]
pub struct ParallelForShape {
    /// Source-level name of the loop counter (e.g. `i` in
    /// `for i in 0..n { … }`).
    pub counter_name: String,
    /// SSA value the header block's first param assigns to
    /// the counter on each iteration entry.
    pub counter_header_value: ValueId,
    /// Integer type of the counter (`i32`, `i64`, …).
    pub counter_ty: Type,
    /// Operand jumped to the header on the first
    /// (entry-edge) jump — i.e. the initial counter value.
    pub start: Operand,
    /// Operand the header compares the counter against.
    pub end: Operand,
    /// Block holding the loop header (cmp + branch to
    /// body/exit). Phi-style block params on this block
    /// carry the loop-loop-carried state.
    pub header_block: BlockId,
    /// Block holding the first instruction of the loop
    /// body (the then-target of the header's branch).
    pub body_block: BlockId,
    /// Block holding the loop's exit (the else-target of
    /// the header's branch). The matching `ParallelForEnd`
    /// hint will be emitted in this block.
    pub exit_block: BlockId,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Instruction {
    pub result: ValueId,
    pub kind: InstrKind,
    pub ty: Type,
    pub span: Span,
}

impl fmt::Display for Instruction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "%{} : {} = ", self.result.0, self.ty)?;
        match &self.kind {
            InstrKind::Const(c) => write!(f, "const {}", c),
            InstrKind::Unary { op, x } => write!(f, "{} {}", op_unary_symbol(*op), x),
            InstrKind::Binary { op, l, r } => write!(f, "{} {} {}", l, op.display_symbol(), r),
            InstrKind::Call { name, args } => {
                write!(f, "call @{}(", name)?;
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{}", a)?;
                }
                f.write_str(")")
            }
            InstrKind::FnRef { name } => write!(f, "fn_ref @{}", name),
            InstrKind::CallIndirect { callee, args } => {
                write!(f, "call_indirect {}(", callee)?;
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{}", a)?;
                }
                f.write_str(")")
            }
            InstrKind::Cast { x, to } => write!(f, "cast {} to {}", x, to),
            InstrKind::StrLit(s) => write!(f, "str_lit {:?}", s),
            InstrKind::ArrayLit { elements } => {
                f.write_str("array_lit [")?;
                for (i, e) in elements.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{}", e)?;
                }
                f.write_str("]")
            }
            InstrKind::Index { array, index, checked } => {
                write!(
                    f,
                    "index{} {}, {}",
                    if *checked { " checked" } else { "" },
                    array,
                    index
                )
            }
            InstrKind::Len { array, length } => write!(f, "len {} /* {} */", array, length),
            InstrKind::RefOf { source, mut_: m } => {
                write!(f, "ref{} {}", if *m { "_mut" } else { "" }, source)
            }
            InstrKind::IndexAssign { array, index, value, checked, .. } => write!(
                f,
                "index_assign{} {}, {}, {}",
                if *checked { " checked" } else { "" },
                array,
                index,
                value
            ),
            InstrKind::Drop { name, ty, .. } => {
                write!(f, "drop {} : {}", name, ty)
            }
            InstrKind::Hint(kind) => match kind {
                HintKind::ParallelForBegin { reductions, .. } => {
                    f.write_str("hint parallel_for_begin")?;
                    if !reductions.is_empty() {
                        f.write_str(" reduce[")?;
                        for (i, (name, op, _)) in reductions.iter().enumerate() {
                            if i > 0 {
                                f.write_str(", ")?;
                            }
                            write!(f, "{} with {}", name, op.display_symbol())?;
                        }
                        f.write_str("]")?;
                    }
                    Ok(())
                }
                HintKind::ParallelForEnd => f.write_str("hint parallel_for_end"),
                HintKind::TaskBegin { handle } => write!(f, "hint task_begin {}", handle),
                HintKind::TaskEnd { handle } => write!(f, "hint task_end {}", handle),
                HintKind::TaskJoin { handle } => write!(f, "hint task_join {}", handle),
            },
        }
    }
}

fn op_unary_symbol(op: UnaryOp) -> &'static str {
    match op {
        UnaryOp::Neg => "-",
        UnaryOp::Not => "!",
    }
}

/// How a basic block hands off control. Each `Branch` and
/// `Jump` carries the argument values that fill the target
/// block's parameters — the SSA equivalent of a phi node's
/// per-predecessor entry.
#[derive(Clone, Debug, PartialEq)]
pub enum Terminator {
    /// `return <maybe-value>` — function exit.
    Return(Option<Operand>),
    /// `jump bb<N>(args…)` — unconditional transfer.
    Jump { target: BlockId, args: Vec<Operand> },
    /// `br <cond>, bb<then>(then_args), bb<else>(else_args)`.
    Branch {
        cond: Operand,
        then_block: BlockId,
        then_args: Vec<Operand>,
        else_block: BlockId,
        else_args: Vec<Operand>,
    },
    /// `unreachable` — placed at e.g. the fall-through arm of an
    /// `assert` after the abort or at the end of an `if/else`
    /// where both arms terminate. Backends will lower to
    /// `llvm.unreachable` / `abort()`.
    Unreachable,
}

impl fmt::Display for Terminator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Terminator::Return(None) => f.write_str("return"),
            Terminator::Return(Some(v)) => write!(f, "return {}", v),
            Terminator::Jump { target, args } => {
                write!(f, "jump bb{}", target.0)?;
                fmt_args(f, args)
            }
            Terminator::Branch {
                cond,
                then_block,
                then_args,
                else_block,
                else_args,
            } => {
                write!(f, "br {}, bb{}", cond, then_block.0)?;
                fmt_args(f, then_args)?;
                write!(f, ", bb{}", else_block.0)?;
                fmt_args(f, else_args)
            }
            Terminator::Unreachable => f.write_str("unreachable"),
        }
    }
}

fn fmt_args(f: &mut fmt::Formatter<'_>, args: &[Operand]) -> fmt::Result {
    if args.is_empty() {
        return Ok(());
    }
    f.write_str("(")?;
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            f.write_str(", ")?;
        }
        write!(f, "{}", a)?;
    }
    f.write_str(")")
}

#[derive(Clone, Debug, PartialEq)]
pub struct BasicBlock {
    pub id: BlockId,
    /// Block parameters — values supplied by every predecessor's
    /// branch terminator. Each parameter has an SSA name + type.
    pub params: Vec<(ValueId, Type)>,
    pub instructions: Vec<Instruction>,
    /// Every block must end with a terminator. The builder uses
    /// `Option<Terminator>` internally while building; here we
    /// require it to be present.
    pub terminator: Terminator,
}

impl fmt::Display for BasicBlock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bb{}", self.id.0)?;
        if !self.params.is_empty() {
            f.write_str("(")?;
            for (i, (v, ty)) in self.params.iter().enumerate() {
                if i > 0 {
                    f.write_str(", ")?;
                }
                write!(f, "%{} : {}", v.0, ty)?;
            }
            f.write_str(")")?;
        }
        f.write_str(":\n")?;
        for instr in &self.instructions {
            writeln!(f, "  {}", instr)?;
        }
        writeln!(f, "  {}", self.terminator)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Function {
    pub name: String,
    pub params: Vec<(String, Type, ValueId)>,
    pub return_type: Type,
    pub entry: BlockId,
    pub blocks: Vec<BasicBlock>,
}

impl fmt::Display for Function {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "fn @{}(", self.name)?;
        for (i, (n, t, v)) in self.params.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "%{} : {} /* {} */", v.0, t, n)?;
        }
        writeln!(f, ") -> {} {{", self.return_type)?;
        for bb in &self.blocks {
            write!(f, "{}", bb)?;
        }
        f.write_str("}\n")
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Module {
    pub functions: Vec<Function>,
}

impl fmt::Display for Module {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for fnc in &self.functions {
            write!(f, "{}", fnc)?;
            f.write_str("\n")?;
        }
        Ok(())
    }
}

/// Error emitted when the lowerer hits a TypedStmt or
/// TypedExprKind that this milestone doesn't handle. Callers
/// (tests, future analyses) should treat this as "the program
/// uses a feature outside the v1 SSA subset" — not a compiler
/// bug — and either skip the function or upgrade the lowerer.
#[derive(Clone, Debug, PartialEq)]
pub struct LowerError {
    pub message: String,
    pub span: Span,
}

impl fmt::Display for LowerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ssa lower: {} (at byte {}..{})",
            self.message, self.span.start, self.span.end
        )
    }
}

/// Lower an entire typed program to SSA. Functions that hit the
/// unsupported subset are skipped — their `LowerError` is
/// returned alongside the partial `Module` so callers can show a
/// "the SSA layer doesn't yet cover X" message without failing
/// the whole compile.
pub fn lower_program(program: &TypedProgram) -> (Module, Vec<LowerError>) {
    let mut module = Module { functions: Vec::new() };
    let mut errors = Vec::new();
    for f in &program.functions {
        match lower_function(f) {
            Ok(fn_ssa) => module.functions.push(fn_ssa),
            Err(e) => errors.push(e),
        }
    }
    (module, errors)
}

pub fn lower_function(f: &TypedFunction) -> Result<Function, LowerError> {
    let mut b = FunctionBuilder::new(f.name.clone(), f.return_type.clone());

    let entry = b.new_block();
    b.set_current(entry);
    let mut locals: Locals = BTreeMap::new();
    let mut param_records = Vec::new();
    for param in &f.params {
        let v = b.fresh_value();
        b.add_block_param(entry, v, param.ty.clone());
        locals.insert(param.name.clone(), v);
        param_records.push((param.name.clone(), param.ty.clone(), v));
    }
    b.params = param_records;
    b.entry = entry;

    lower_stmts(&f.body, &mut b, &mut locals)?;

    // If we fell off the end of the body without a terminator,
    // the checker has already flagged it (every well-typed
    // function ends with a return on every path), but defend
    // anyway: terminate with `unreachable` so the IR is
    // structurally valid.
    if b.current_block_terminator().is_none() {
        b.terminate(Terminator::Unreachable);
    }
    Ok(b.build())
}

type Locals = BTreeMap<String, ValueId>;

fn lower_stmts(
    stmts: &[TypedStmt],
    b: &mut FunctionBuilder,
    locals: &mut Locals,
) -> Result<Vec<String>, LowerError> {
    // Track names introduced via top-level `let` in this stmt
    // list so callers (lower_if / lower_while / lower_for_iter)
    // can distinguish inner-scope let-shadows from genuine
    // reassignments and restore shadowed entries to their
    // entry-scope values before merging back.
    let mut let_introduced: Vec<String> = Vec::new();
    for stmt in stmts {
        if b.current_block_terminator().is_some() {
            break;
        }
        if let TypedStmt::Let { name, .. } = stmt {
            let_introduced.push(name.clone());
        }
        lower_stmt(stmt, b, locals)?;
    }
    Ok(let_introduced)
}

fn lower_stmt(
    stmt: &TypedStmt,
    b: &mut FunctionBuilder,
    locals: &mut Locals,
) -> Result<(), LowerError> {
    match stmt {
        TypedStmt::Let { name, expr, .. } => {
            let v = lower_expr_to_value(expr, b, locals)?;
            locals.insert(name.clone(), v);
            Ok(())
        }
        TypedStmt::Reassign { name, ty, expr, drop_old } => {
            // Closure #134: SSA now lowers `drop_old` reassigns
            // for OwnedStr / Vec — evaluate the RHS first, then
            // emit a Drop of the OLD value (so any RHS that
            // READS the binding still sees the live buffer),
            // then update locals to the new SSA value. The
            // backends' Drop emit handlers handle the actual
            // free. Other non-Copy reassigns still surface as
            // a LowerError (the tree backends model those
            // shapes; SSA doesn't yet).
            if *drop_old && !matches!(ty, Type::OwnedStr | Type::Vec(_)) {
                return Err(LowerError {
                    message: format!(
                        "reassign of '{}' over a non-Copy value is not in the v1 SSA subset",
                        name
                    ),
                    span: expr.span,
                });
            }
            let new_v = lower_expr_to_value(expr, b, locals)?;
            if *drop_old {
                if let Some(old) = locals.get(name).copied() {
                    b.emit(
                        Type::I64,
                        expr.span,
                        InstrKind::Drop {
                            source: Operand::Value(old),
                            name: name.clone(),
                            ty: ty.clone(),
                        },
                    );
                }
            }
            locals.insert(name.clone(), new_v);
            Ok(())
        }
        TypedStmt::Return { expr } => {
            let v = lower_expr_to_operand(expr, b, locals)?;
            b.terminate(Terminator::Return(Some(v)));
            Ok(())
        }
        TypedStmt::Assert { expr, message } => {
            let cond = lower_expr_to_operand(expr, b, locals)?;
            let ok = b.new_block();
            let fail = b.new_block();
            b.terminate(Terminator::Branch {
                cond,
                then_block: ok,
                then_args: Vec::new(),
                else_block: fail,
                else_args: Vec::new(),
            });
            b.set_current(fail);
            // If the assertion carried a user-facing message,
            // emit a call to the runtime abort helper with the
            // message as a literal string operand. Otherwise the
            // fail block just terminates as unreachable — the
            // SMT pass should already have proven it can't be
            // reached.
            if let Some(msg) = message {
                let msg_v = b.emit(
                    Type::Str,
                    expr.span,
                    InstrKind::StrLit(msg.clone()),
                );
                b.emit(
                    Type::I64,
                    expr.span,
                    InstrKind::Call {
                        name: "intent_assert_fail".to_string(),
                        args: vec![Operand::Value(msg_v)],
                    },
                );
            }
            b.terminate(Terminator::Unreachable);
            b.set_current(ok);
            Ok(())
        }
        TypedStmt::If { cond, then_body, else_body } => {
            lower_if(cond, then_body, else_body, b, locals)
        }
        TypedStmt::While { cond, body } => lower_while(cond, body, b, locals),
        TypedStmt::For { var, ty, start, end, body, parallel, reductions } => {
            if *parallel {
                // Sequential lowering preserves correctness
                // (the verifier already proved race-freedom).
                // We bracket the for-range with hint markers so
                // backends and analyses can still recognize
                // the parallel-region shape. The
                // `ParallelForBegin` hint also carries the
                // structured-loop shape so backends can emit
                // OpenMP / GOMP without re-walking the CFG.
                let red_meta: Vec<(String, crate::ast::ReductionOp, Type)> = reductions
                    .iter()
                    .map(|r| (r.var.clone(), r.op, r.ty.clone()))
                    .collect();
                // Emit a placeholder Begin in the current
                // (pre-header) block; after
                // `lower_integer_for` returns the
                // structured-loop shape, patch the placeholder
                // with the real values. Emitting Begin first
                // keeps the hint physically located in the
                // pre-header — the position
                // `ssa_pass::recognize_reduction_shapes`
                // already uses to identify `begin_block`.
                let pre_block = b.current;
                let begin_idx = b.blocks[pre_block.0 as usize].instructions.len();
                let placeholder_shape = ParallelForShape {
                    counter_name: var.clone(),
                    counter_header_value: ValueId(0),
                    counter_ty: ty.clone(),
                    start: Operand::Const(Const::Int(0)),
                    end: Operand::Const(Const::Int(0)),
                    header_block: BlockId(0),
                    body_block: BlockId(0),
                    exit_block: BlockId(0),
                };
                b.emit(
                    Type::I64,
                    start.span,
                    InstrKind::Hint(HintKind::ParallelForBegin {
                        reductions: red_meta,
                        shape: placeholder_shape,
                    }),
                );
                let shape_info = lower_integer_for(var, ty, start, end, body, b, locals)?;
                let real_shape = ParallelForShape {
                    counter_name: shape_info.counter_name,
                    counter_header_value: shape_info.counter_header_value,
                    counter_ty: shape_info.counter_ty,
                    start: shape_info.start,
                    end: shape_info.end,
                    header_block: shape_info.header,
                    body_block: shape_info.body,
                    exit_block: shape_info.exit,
                };
                match &mut b.blocks[pre_block.0 as usize].instructions[begin_idx].kind {
                    InstrKind::Hint(HintKind::ParallelForBegin { shape, .. }) => {
                        *shape = real_shape;
                    }
                    other => unreachable!(
                        "expected ParallelForBegin placeholder at pre-header[{}], got {:?}",
                        begin_idx, other
                    ),
                }
                b.emit(
                    Type::I64,
                    end.span,
                    InstrKind::Hint(HintKind::ParallelForEnd),
                );
                Ok(())
            } else {
                lower_integer_for(var, ty, start, end, body, b, locals).map(|_| ())
            }
        }
        TypedStmt::TaskSpawn { name, body, .. } => {
            b.emit(
                Type::I64,
                Span::default(),
                InstrKind::Hint(HintKind::TaskBegin {
                    handle: name.clone(),
                }),
            );
            // Body runs inline (v1 sequential lowering). New
            // bindings inside don't escape; we use a snapshot
            // of locals so any Reassign in the body is local.
            let mut body_locals = locals.clone();
            lower_stmts(body, b, &mut body_locals)?;
            b.emit(
                Type::I64,
                Span::default(),
                InstrKind::Hint(HintKind::TaskEnd {
                    handle: name.clone(),
                }),
            );
            // Record the task handle in outer locals so the
            // matching `join` can find it. The handle's SSA
            // value is the TaskEnd instruction's result (a
            // marker — analyses can ignore it).
            // We store a Const(Int(0)) operand for the handle
            // — there's no observable runtime state.
            let dummy = b.emit(
                Type::I64,
                Span::default(),
                InstrKind::Const(Const::Int(0)),
            );
            locals.insert(name.clone(), dummy);
            Ok(())
        }
        TypedStmt::TaskJoin { name } => {
            b.emit(
                Type::I64,
                Span::default(),
                InstrKind::Hint(HintKind::TaskJoin {
                    handle: name.clone(),
                }),
            );
            Ok(())
        }
        TypedStmt::Break => {
            let Some(frame) = b.loops.last().cloned() else {
                return Err(LowerError {
                    message: "break outside any loop reached the SSA lowerer".into(),
                    span: Span::default(),
                });
            };
            let args = loop_carry_args(&frame, locals);
            b.terminate(Terminator::Jump {
                target: frame.exit,
                args,
            });
            Ok(())
        }
        TypedStmt::Continue => {
            let Some(frame) = b.loops.last().cloned() else {
                return Err(LowerError {
                    message: "continue outside any loop reached the SSA lowerer".into(),
                    span: Span::default(),
                });
            };
            let args = loop_carry_args(&frame, locals);
            b.terminate(Terminator::Jump {
                target: frame.header,
                args,
            });
            Ok(())
        }
        TypedStmt::Discard { expr } => {
            // The value is dropped, but any side-effecting
            // subexpressions (asserts, indexing, calls) still
            // run. Lower into an SSA value, then for non-Copy
            // heap-shaped types emit a Drop so the backend
            // releases the buffer (`free` for OwnedStr, the
            // matching `__free` helper for Vec). Closure #134:
            // `let _ = make_owned_str();` was silently leaking
            // because the lowerer just forgot the value.
            let v = lower_expr_to_value(expr, b, locals)?;
            let ty = expr.ty.clone();
            if matches!(ty, Type::OwnedStr | Type::Vec(_)) {
                b.emit(
                    Type::I64,
                    expr.span,
                    InstrKind::Drop {
                        source: Operand::Value(v),
                        name: "_".to_string(),
                        ty,
                    },
                );
            }
            Ok(())
        }
        TypedStmt::Drop { name, ty, moved_fields: _ } => {
            // Look up the binding's current SSA value so the
            // Drop carries an Operand. Missing locals entries
            // are a lowerer bug (Drop is emitted by the
            // checker only for bindings still in scope), so
            // fall back to Const(Int(0)) as a defensive
            // sentinel — the audit pass reads only `name`.
            let source = locals
                .get(name)
                .map(|v| Operand::Value(*v))
                .unwrap_or_else(|| Operand::Const(Const::Int(0)));
            b.emit(
                Type::I64, // result is unit-ish; backend ignores
                Span::default(),
                InstrKind::Drop {
                    source,
                    name: name.clone(),
                    ty: ty.clone(),
                },
            );
            Ok(())
        }
        TypedStmt::FieldAssign { .. } => {
            // Tree backends handle field assignment directly;
            // the SSA path doesn't model struct field writes
            // yet (parallels other struct ops). T1.2 phase 2a.
            Err(LowerError {
                message: "SSA lowering of field assignment is not yet supported"
                    .into(),
                span: crate::span::Span::default(),
            })
        }
        TypedStmt::IndexAssign { name, base_ty, index, field_path, value, checked } => {
            // SSA path doesn't lower mixed index+field assigns
            // yet — route to the tree backend. T1.2 phase 2b
            // follow-up.
            if !field_path.is_empty() {
                return Err(LowerError {
                    message: "SSA lowering of `xs[i].field = …` is not yet \
                              implemented; routing to the tree backend"
                        .to_string(),
                    span: index.span,
                });
            }
            let array = locals
                .get(name)
                .map(|v| Operand::Value(*v))
                .ok_or_else(|| LowerError {
                    message: format!(
                        "IndexAssign target '{}' has no SSA value (unknown binding)",
                        name
                    ),
                    span: index.span,
                })?;
            let i = lower_expr_to_operand(index, b, locals)?;
            let v = lower_expr_to_operand(value, b, locals)?;
            b.emit(
                Type::I64,
                index.span,
                InstrKind::IndexAssign {
                    array,
                    base_ty: base_ty.clone(),
                    index: i,
                    value: v,
                    checked: *checked,
                },
            );
            Ok(())
        }
        TypedStmt::Print { items } => {
            // Mirror tree-LLVM's `emit_print_items` shape:
            //   <item>  [' ' <item>]*  '\n'
            // The lowering emits one `intent_print_item` call
            // per item (no trailing newline; the backend
            // dispatches on the arg type), a
            // `intent_print_putc(' ')` between items, and a
            // single `intent_print_putc('\n')` after all
            // items. `Str` items become a `StrLit` IR
            // instruction whose value is passed to
            // `intent_print_item`.
            let span = items
                .iter()
                .find_map(|it| match it {
                    crate::ir::TypedPrintItem::Expr(e) => Some(e.span),
                    crate::ir::TypedPrintItem::Str(_) => None,
                })
                .unwrap_or_default();
            for (i, item) in items.iter().enumerate() {
                // For OwnedStr items, track whether the operand
                // came from a heap-producing expression we own.
                // Conservative whitelist: Call returning
                // OwnedStr (intent_str_concat / user fn) and
                // Binary `+` (string concat) are the v1 ways
                // to produce a fresh heap-allocated OwnedStr
                // with no other owner. Var / FieldAccess /
                // TupleAccess all reference a value owned by
                // some binding (whose scope-exit Drop frees
                // the heap) — freeing after print would
                // double-free. Closure #135.
                let needs_drop_after_print = match item {
                    crate::ir::TypedPrintItem::Expr(e) => {
                        crate::ir::is_fresh_owned_str(e)
                    }
                    crate::ir::TypedPrintItem::Str(_) => false,
                };
                let op = match item {
                    crate::ir::TypedPrintItem::Expr(e) => {
                        lower_expr_to_operand(e, b, locals)?
                    }
                    crate::ir::TypedPrintItem::Str(text) => {
                        let v = b.emit(
                            Type::Str,
                            span,
                            InstrKind::StrLit(text.clone()),
                        );
                        Operand::Value(v)
                    }
                };
                b.emit(
                    Type::I64,
                    span,
                    InstrKind::Call {
                        name: "intent_print_item".to_string(),
                        args: vec![op.clone()],
                    },
                );
                if needs_drop_after_print {
                    b.emit(
                        Type::I64,
                        span,
                        InstrKind::Drop {
                            source: op,
                            name: "_".to_string(),
                            ty: Type::OwnedStr,
                        },
                    );
                }
                if i + 1 < items.len() {
                    b.emit(
                        Type::I64,
                        span,
                        InstrKind::Call {
                            name: "intent_print_putc".to_string(),
                            args: vec![Operand::Const(Const::Int(32))],
                        },
                    );
                }
            }
            b.emit(
                Type::I64,
                span,
                InstrKind::Call {
                    name: "intent_print_putc".to_string(),
                    args: vec![Operand::Const(Const::Int(10))],
                },
            );
            Ok(())
        }
        TypedStmt::Prove { .. } => {
            // No-op at the SSA layer: prove is discharged at
            // type-check time.
            Ok(())
        }
        TypedStmt::ForIter { var, element_ty, collection, collection_ty: _, consumes, body } => {
            lower_for_iter(var, element_ty, collection, *consumes, body, b, locals)
        }
    }
}

fn lower_for_iter(
    var: &str,
    element_ty: &Type,
    collection: &str,
    _consumes: bool,
    body: &[TypedStmt],
    b: &mut FunctionBuilder,
    locals: &mut Locals,
) -> Result<(), LowerError> {
    // `for x in xs { body }` desugars to a counter loop over
    // `len(xs)` with `x` rebound on each iteration. v1 doesn't
    // distinguish consume from borrow at the SSA layer —
    // backends inspect the IndexAssign / Drop instructions
    // they see to decide whether to free.
    let coll_v = *locals.get(collection).ok_or_else(|| LowerError {
        message: format!("for-iter over undeclared binding '{}'", collection),
        span: Span::default(),
    })?;
    let coll_ty = b.value_type(coll_v).clone();
    // Materialize len(xs).
    let length_const = match coll_ty.deref() {
        Type::Array { length, .. } => *length,
        _ => 0,
    };
    let len_v = b.emit(
        Type::I64,
        Span::default(),
        InstrKind::Len {
            array: Operand::Value(coll_v),
            length: length_const,
        },
    );

    let header = b.new_block();
    let body_bb = b.new_block();
    let exit = b.new_block();

    // Carry: idx counter + any pre-loop bindings modified
    // inside the body (excluding `var` and the implicit idx).
    let i_header = b.fresh_value();
    b.add_block_param(header, i_header, Type::I64);
    let i_exit = b.fresh_value();
    b.add_block_param(exit, i_exit, Type::I64);
    let mut carry: Vec<(String, ValueId, Type)> = vec![(
        format!("__intent_iter_idx_{}", var),
        i_header,
        Type::I64,
    )];
    let modified = modified_in_body(body);
    for name in &modified {
        if name == var {
            continue;
        }
        if let Some(entry_v) = locals.get(name).copied() {
            let cty = b.value_type(entry_v).clone();
            let hp = b.fresh_value();
            b.add_block_param(header, hp, cty.clone());
            let ep = b.fresh_value();
            b.add_block_param(exit, ep, cty.clone());
            carry.push((name.clone(), hp, cty));
        }
    }

    // Entry → header with idx=0 + pre-loop carry values.
    let mut entry_args: Vec<Operand> = vec![Operand::Const(Const::Int(0))];
    for (name, _, _) in carry.iter().skip(1) {
        entry_args.push(Operand::Value(*locals.get(name).expect("carry in entry")));
    }
    b.terminate(Terminator::Jump {
        target: header,
        args: entry_args,
    });

    // Header: idx < len.
    b.set_current(header);
    let cmp = b.emit(
        Type::Bool,
        Span::default(),
        InstrKind::Binary {
            op: BinaryOp::Lt,
            l: Operand::Value(i_header),
            r: Operand::Value(len_v),
        },
    );
    let header_to_exit: Vec<Operand> = carry
        .iter()
        .map(|(_, hp, _)| Operand::Value(*hp))
        .collect();
    b.terminate(Terminator::Branch {
        cond: Operand::Value(cmp),
        then_block: body_bb,
        then_args: Vec::new(),
        else_block: exit,
        else_args: header_to_exit,
    });

    // Body: load xs[idx] into `var`, then run the body.
    b.set_current(body_bb);
    let element_v = b.emit(
        element_ty.clone(),
        Span::default(),
        InstrKind::Index {
            array: Operand::Value(coll_v),
            index: Operand::Value(i_header),
            checked: false, // idx < len holds by construction
        },
    );
    let mut body_locals = locals.clone();
    for (name, hp, _) in &carry {
        body_locals.insert(name.clone(), *hp);
    }
    body_locals.insert(var.to_string(), element_v);
    b.loops.push(LoopFrame {
        header,
        exit,
        carry: carry.clone(),
    });
    lower_stmts(body, b, &mut body_locals)?;
    if b.current_block_terminator().is_none() {
        let inc = b.emit(
            Type::I64,
            Span::default(),
            InstrKind::Binary {
                op: BinaryOp::Add,
                l: Operand::Value(i_header),
                r: Operand::Const(Const::Int(1)),
            },
        );
        let mut back_args: Vec<Operand> = vec![Operand::Value(inc)];
        for (name, _, _) in carry.iter().skip(1) {
            back_args.push(Operand::Value(
                *body_locals.get(name).expect("carry in body"),
            ));
        }
        b.terminate(Terminator::Jump {
            target: header,
            args: back_args,
        });
    }
    b.loops.pop();

    // Continue past the loop in `exit`, rebinding carry.
    b.set_current(exit);
    let exit_params: Vec<(ValueId, Type)> = b.blocks[exit.0 as usize].params.clone();
    for ((name, _, _), (exit_v, _)) in carry.iter().zip(exit_params.iter()) {
        if name.starts_with("__intent_iter_idx_") {
            continue;
        }
        locals.insert(name.clone(), *exit_v);
    }
    Ok(())
}

#[allow(dead_code)]
fn stmt_kind_name(stmt: &TypedStmt) -> &'static str {
    match stmt {
        TypedStmt::Let { .. } => "Let",
        TypedStmt::Reassign { .. } => "Reassign",
        TypedStmt::Drop { .. } => "Drop",
        TypedStmt::Return { .. } => "Return",
        TypedStmt::Assert { .. } => "Assert",
        TypedStmt::Prove { .. } => "Prove",
        TypedStmt::Print { .. } => "Print",
        TypedStmt::If { .. } => "If",
        TypedStmt::While { .. } => "While",
        TypedStmt::Discard { .. } => "Discard",
        TypedStmt::Break => "Break",
        TypedStmt::Continue => "Continue",
        TypedStmt::IndexAssign { .. } => "IndexAssign",
        TypedStmt::FieldAssign { .. } => "FieldAssign",
        TypedStmt::For { .. } => "For",
        TypedStmt::ForIter { .. } => "ForIter",
        TypedStmt::TaskSpawn { .. } => "TaskSpawn",
        TypedStmt::TaskJoin { .. } => "TaskJoin",
    }
}

/// Loop-frame entry maintained on `FunctionBuilder::loops` while
/// lowering a `while` / `for` body. Break and Continue read this
/// to find their target block and what loop-carried bindings need
/// to be passed as block-args.
#[derive(Clone, Debug)]
struct LoopFrame {
    /// Block that re-evaluates the loop condition (and the
    /// target of `continue`).
    header: BlockId,
    /// Block after the loop (and the target of `break`).
    exit: BlockId,
    /// Bindings carried across loop iterations. Ordered so the
    /// args list at every jump site uses a stable order.
    /// Each entry: (binding name, header-block-arg ValueId,
    /// type). The exit block takes the same params in the same
    /// order — so break jumps and the normal "header observed
    /// cond false" path agree on the merge.
    carry: Vec<(String, ValueId, Type)>,
}

fn loop_carry_args(frame: &LoopFrame, locals: &Locals) -> Vec<Operand> {
    frame
        .carry
        .iter()
        .map(|(name, _arg_v, _)| {
            let v = locals.get(name).copied().expect("loop-carried binding present");
            Operand::Value(v)
        })
        .collect()
}

/// Walk a TypedStmt body and collect names of pre-loop bindings
/// that are reassigned (or IndexAssign-targeted) anywhere in
/// the body — recursively. Bindings introduced inside the body
/// (Let) don't escape and are excluded. The set is used to pick
/// loop-carried block-args.
fn modified_in_body(body: &[TypedStmt]) -> std::collections::BTreeSet<String> {
    fn walk(stmts: &[TypedStmt], inner: &mut std::collections::BTreeSet<String>,
            out: &mut std::collections::BTreeSet<String>) {
        for s in stmts {
            match s {
                TypedStmt::Let { name, expr, .. } => {
                    walk_expr_reads(expr, out);
                    inner.insert(name.clone());
                }
                TypedStmt::Reassign { name, expr, .. } => {
                    walk_expr_reads(expr, out);
                    if !inner.contains(name) {
                        out.insert(name.clone());
                    }
                }
                TypedStmt::IndexAssign { name, index, value, .. } => {
                    walk_expr_reads(index, out);
                    walk_expr_reads(value, out);
                    if !inner.contains(name) {
                        out.insert(name.clone());
                    }
                }
                TypedStmt::FieldAssign { object, value, .. } => {
                    walk_expr_reads(object, out);
                    walk_expr_reads(value, out);
                }
                TypedStmt::Return { expr }
                | TypedStmt::Assert { expr, .. }
                | TypedStmt::Discard { expr }
                | TypedStmt::Prove { expr } => walk_expr_reads(expr, out),
                TypedStmt::Print { items } => {
                    for it in items {
                        if let crate::ir::TypedPrintItem::Expr(e) = it {
                            walk_expr_reads(e, out);
                        }
                    }
                }
                TypedStmt::If { cond, then_body, else_body } => {
                    walk_expr_reads(cond, out);
                    walk(then_body, inner, out);
                    walk(else_body, inner, out);
                }
                TypedStmt::While { cond, body } => {
                    walk_expr_reads(cond, out);
                    walk(body, inner, out);
                }
                TypedStmt::For { start, end, body, .. } => {
                    walk_expr_reads(start, out);
                    walk_expr_reads(end, out);
                    walk(body, inner, out);
                }
                TypedStmt::ForIter { body, .. } => {
                    walk(body, inner, out);
                }
                TypedStmt::TaskSpawn { body, .. } => {
                    walk(body, inner, out);
                }
                _ => {}
            }
        }
    }
    fn walk_expr_reads(_e: &TypedExpr, _out: &mut std::collections::BTreeSet<String>) {
        // Reads don't modify bindings; nothing to collect for the
        // purpose of loop-carry. (Kept as a hook in case a future
        // expression form has side effects we need to track.)
    }
    let mut out = std::collections::BTreeSet::new();
    let mut inner = std::collections::BTreeSet::new();
    walk(body, &mut inner, &mut out);
    out
}

fn lower_while(
    cond: &TypedExpr,
    body: &[TypedStmt],
    b: &mut FunctionBuilder,
    locals: &mut Locals,
) -> Result<(), LowerError> {
    let header = b.new_block();
    let body_bb = b.new_block();
    let exit = b.new_block();

    // Loop-carried bindings: any pre-loop binding the body
    // reassigns. Each becomes a block-param on header (and on
    // exit, to receive break-jumped values).
    let modified = modified_in_body(body);
    let carry: Vec<(String, ValueId, Type)> = modified
        .iter()
        .filter_map(|name| {
            locals.get(name).map(|entry_v| {
                let ty = b.value_type(*entry_v).clone();
                let header_param = b.fresh_value();
                b.add_block_param(header, header_param, ty.clone());
                let exit_param = b.fresh_value();
                b.add_block_param(exit, exit_param, ty.clone());
                (name.clone(), header_param, ty)
            })
        })
        .collect();

    // Map each carried binding's exit-block param ValueId so we
    // can rewrite locals after the loop. exit_params is stored
    // parallel to carry (same order).
    let exit_params: Vec<ValueId> =
        carry.iter().map(|_| ValueId(0)).collect();
    // ^ unused placeholder — we'll capture exit params directly
    // from the exit block. Simpler: re-fetch exit's params after
    // construction.
    let _ = exit_params;

    // Entry jumps to header passing pre-loop values.
    let entry_args: Vec<Operand> = carry
        .iter()
        .map(|(name, _, _)| Operand::Value(*locals.get(name).expect("carry name in locals")))
        .collect();
    b.terminate(Terminator::Jump {
        target: header,
        args: entry_args,
    });

    // Inside the loop, the carried bindings refer to the header
    // block-args (not their pre-loop values).
    let mut loop_locals = locals.clone();
    for (name, header_param, _) in &carry {
        loop_locals.insert(name.clone(), *header_param);
    }

    // Header: evaluate cond, branch to body / exit. Both
    // branches forward the same set of values that we already
    // bound via header's params.
    b.set_current(header);
    let c = lower_expr_to_operand(cond, b, &mut loop_locals)?;
    let header_to_exit_args: Vec<Operand> = carry
        .iter()
        .map(|(_, hp, _)| Operand::Value(*hp))
        .collect();
    b.terminate(Terminator::Branch {
        cond: c,
        then_block: body_bb,
        then_args: Vec::new(),
        else_block: exit,
        else_args: header_to_exit_args,
    });

    // Body: lower with carried bindings visible. Track loop
    // frame so break/continue route correctly.
    b.set_current(body_bb);
    b.loops.push(LoopFrame {
        header,
        exit,
        carry: carry.clone(),
    });
    let mut body_locals = loop_locals.clone();
    lower_stmts(body, b, &mut body_locals)?;
    // After the body, if it didn't terminate, jump back to
    // header with the body's final carry values.
    if b.current_block_terminator().is_none() {
        let back_args: Vec<Operand> = carry
            .iter()
            .map(|(name, _, _)| {
                Operand::Value(*body_locals.get(name).expect("carry name in body locals"))
            })
            .collect();
        b.terminate(Terminator::Jump {
            target: header,
            args: back_args,
        });
    }
    b.loops.pop();

    // Continue lowering after the loop in `exit`, with carried
    // bindings rebound to exit's block-args.
    b.set_current(exit);
    let exit_params: Vec<(ValueId, Type)> = b.blocks[exit.0 as usize].params.clone();
    for ((name, _, _), (exit_v, _ty)) in carry.iter().zip(exit_params.iter()) {
        locals.insert(name.clone(), *exit_v);
    }
    Ok(())
}

/// Shape info captured by `lower_integer_for`. Callers that
/// need the structured-loop form (the parallel-for arm of
/// `TypedStmt::For` populates this into the begin-hint so
/// backends can emit OpenMP / GOMP without CFG pattern
/// recognition) consume the values; sequential callers
/// discard via `let _`.
#[derive(Clone, Debug)]
struct IntegerForShape {
    counter_name: String,
    counter_header_value: ValueId,
    counter_ty: Type,
    start: Operand,
    end: Operand,
    header: BlockId,
    body: BlockId,
    exit: BlockId,
}

fn lower_integer_for(
    var: &str,
    ty: &Type,
    start: &TypedExpr,
    end: &TypedExpr,
    body: &[TypedStmt],
    b: &mut FunctionBuilder,
    locals: &mut Locals,
) -> Result<IntegerForShape, LowerError> {
    // Lower `for i in start..end { body }` as the equivalent
    // counter-while form: `let i = start; while i < end { body;
    // i = i + 1; }`. Reuses the same loop-carried block-arg
    // construction as `lower_while`.
    let start_v = lower_expr_to_value(start, b, locals)?;
    let end_op = lower_expr_to_operand(end, b, locals)?;

    let header = b.new_block();
    let body_bb = b.new_block();
    let exit = b.new_block();

    // Loop-carried bindings include the loop variable itself
    // plus anything the body reassigns. Build the list with the
    // loop var FIRST so test inspection is more predictable.
    let modified = modified_in_body(body);
    let mut carry: Vec<(String, ValueId, Type)> = Vec::new();
    // Loop variable (always carried).
    let i_header = b.fresh_value();
    b.add_block_param(header, i_header, ty.clone());
    let i_exit = b.fresh_value();
    b.add_block_param(exit, i_exit, ty.clone());
    carry.push((var.to_string(), i_header, ty.clone()));
    // Any pre-loop bindings the body reassigns.
    for name in &modified {
        if name == var {
            continue;
        }
        if let Some(entry_v) = locals.get(name).copied() {
            let cty = b.value_type(entry_v).clone();
            let hp = b.fresh_value();
            b.add_block_param(header, hp, cty.clone());
            let ep = b.fresh_value();
            b.add_block_param(exit, ep, cty.clone());
            carry.push((name.clone(), hp, cty));
        }
    }

    // Jump to header from entry with start + pre-loop carry
    // values.
    let mut entry_args: Vec<Operand> = vec![Operand::Value(start_v)];
    for (name, _, _) in carry.iter().skip(1) {
        entry_args.push(Operand::Value(*locals.get(name).expect("carry name in locals")));
    }
    b.terminate(Terminator::Jump {
        target: header,
        args: entry_args,
    });

    // Inside the loop body, locals see the header-param
    // ValueIds for carried bindings (including the loop var).
    let mut loop_locals = locals.clone();
    for (name, hp, _) in &carry {
        loop_locals.insert(name.clone(), *hp);
    }

    // Header: cmp i < end, branch.
    b.set_current(header);
    let cmp_op = if ty.is_signed_integer() {
        BinaryOp::Lt
    } else {
        BinaryOp::Lt // checker uses one BinaryOp; signedness is the operand's
    };
    let cmp_v = b.emit(
        Type::Bool,
        start.span,
        InstrKind::Binary {
            op: cmp_op,
            l: Operand::Value(i_header),
            r: end_op.clone(),
        },
    );
    // The exit-edge from header forwards i_header (as the
    // final value of the loop var) and the pre-loop carry
    // values' current header params.
    let header_to_exit: Vec<Operand> = carry
        .iter()
        .map(|(_, hp, _)| Operand::Value(*hp))
        .collect();
    b.terminate(Terminator::Branch {
        cond: Operand::Value(cmp_v),
        then_block: body_bb,
        then_args: Vec::new(),
        else_block: exit,
        else_args: header_to_exit,
    });

    // Body: lower with carry visible, set up the loop frame.
    b.set_current(body_bb);
    b.loops.push(LoopFrame {
        header,
        exit,
        carry: carry.clone(),
    });
    let mut body_locals = loop_locals.clone();
    lower_stmts(body, b, &mut body_locals)?;

    // After the body (if it didn't terminate), emit `i = i + 1`
    // and jump back to header with the incremented value plus
    // current carry values.
    if b.current_block_terminator().is_none() {
        let inc = b.emit(
            ty.clone(),
            end.span,
            InstrKind::Binary {
                op: BinaryOp::Add,
                l: Operand::Value(*body_locals.get(var).expect("loop var in body locals")),
                r: Operand::Const(Const::Int(1)),
            },
        );
        body_locals.insert(var.to_string(), inc);
        let back_args: Vec<Operand> = carry
            .iter()
            .map(|(name, _, _)| {
                Operand::Value(*body_locals.get(name).expect("carry in body"))
            })
            .collect();
        b.terminate(Terminator::Jump {
            target: header,
            args: back_args,
        });
    }
    b.loops.pop();

    // Rebind carried bindings (except the loop var, which goes
    // out of scope at the loop's end) to exit-block params.
    b.set_current(exit);
    let exit_params: Vec<(ValueId, Type)> = b.blocks[exit.0 as usize].params.clone();
    for ((name, _, _), (exit_v, _)) in carry.iter().zip(exit_params.iter()) {
        if name != var {
            locals.insert(name.clone(), *exit_v);
        }
    }
    Ok(IntegerForShape {
        counter_name: var.to_string(),
        counter_header_value: i_header,
        counter_ty: ty.clone(),
        start: Operand::Value(start_v),
        end: end_op,
        header,
        body: body_bb,
        exit,
    })
}

fn lower_if(
    cond: &TypedExpr,
    then_body: &[TypedStmt],
    else_body: &[TypedStmt],
    b: &mut FunctionBuilder,
    locals: &mut Locals,
) -> Result<(), LowerError> {
    let c = lower_expr_to_operand(cond, b, locals)?;
    let then_bb = b.new_block();
    let else_bb = b.new_block();

    // Snapshot the locals so each branch can mutate
    // independently; we'll diff them at the merge.
    let entry_locals = locals.clone();

    b.terminate(Terminator::Branch {
        cond: c,
        then_block: then_bb,
        then_args: Vec::new(),
        else_block: else_bb,
        else_args: Vec::new(),
    });

    // Then branch.
    b.set_current(then_bb);
    let mut then_locals = entry_locals.clone();
    let then_introduced = lower_stmts(then_body, b, &mut then_locals)?;
    let then_tail = b.current_block();
    let then_terminated = b.current_block_terminator().is_some();
    // Restore shadowed entries: any name `let`-introduced in
    // this branch that ALSO existed at entry is an inner-scope
    // shadow, not a genuine reassignment. Pin its end-of-branch
    // value back to the entry value so the merge below won't
    // generate a spurious phi that leaks the inner SSA value
    // out to the outer scope.
    for name in &then_introduced {
        if let Some(entry_v) = entry_locals.get(name).copied() {
            then_locals.insert(name.clone(), entry_v);
        }
    }

    // Else branch.
    b.set_current(else_bb);
    let mut else_locals = entry_locals.clone();
    let else_introduced = lower_stmts(else_body, b, &mut else_locals)?;
    let else_tail = b.current_block();
    let else_terminated = b.current_block_terminator().is_some();
    for name in &else_introduced {
        if let Some(entry_v) = entry_locals.get(name).copied() {
            else_locals.insert(name.clone(), entry_v);
        }
    }

    // Common case: at least one branch falls through to a
    // merge block. The merge takes a block parameter for every
    // binding whose SSA name differs between the branches OR
    // (for safety) for every binding modified in either branch.
    if then_terminated && else_terminated {
        // Both branches returned/aborted — no merge needed.
        // Caller continues on an unreachable path (the next
        // stmt loop iteration will see the now-terminated
        // current block and stop).
        return Ok(());
    }

    let merge_bb = b.new_block();

    // Build the set of bindings that need merging: any whose
    // value in then-branch or else-branch differs from the
    // entry value (we only carry bindings present in both
    // branches; new bindings introduced inside a branch don't
    // escape the merge).
    let mut merged: Vec<String> = Vec::new();
    for (name, entry_v) in &entry_locals {
        let t = then_locals.get(name).copied().unwrap_or(*entry_v);
        let e = else_locals.get(name).copied().unwrap_or(*entry_v);
        if t != e {
            merged.push(name.clone());
        }
    }
    merged.sort();

    // For each merged binding, the merge block gets one param.
    let mut merge_params: Vec<(String, ValueId, Type)> = Vec::new();
    for name in &merged {
        let entry_v = entry_locals.get(name).copied().expect("merged binding exists in entry");
        let ty = b.value_type(entry_v).clone();
        let v = b.fresh_value();
        b.add_block_param(merge_bb, v, ty.clone());
        merge_params.push((name.clone(), v, ty));
    }

    // Then-tail jumps to merge with then-side values for each
    // merged binding (or the entry value if the then branch
    // didn't touch it).
    if !then_terminated {
        let args: Vec<Operand> = merged
            .iter()
            .map(|name| {
                let v = then_locals
                    .get(name)
                    .copied()
                    .or_else(|| entry_locals.get(name).copied())
                    .expect("merged binding has a then-side value");
                Operand::Value(v)
            })
            .collect();
        b.set_current(then_tail);
        b.terminate(Terminator::Jump {
            target: merge_bb,
            args,
        });
    }
    if !else_terminated {
        let args: Vec<Operand> = merged
            .iter()
            .map(|name| {
                let v = else_locals
                    .get(name)
                    .copied()
                    .or_else(|| entry_locals.get(name).copied())
                    .expect("merged binding has an else-side value");
                Operand::Value(v)
            })
            .collect();
        b.set_current(else_tail);
        b.terminate(Terminator::Jump {
            target: merge_bb,
            args,
        });
    }

    // The merged block-arg ValueIds become the new SSA names
    // for those bindings going forward.
    b.set_current(merge_bb);
    for (name, v, _ty) in merge_params {
        locals.insert(name, v);
    }
    Ok(())
}

/// Lower an expression, materializing its result as a fresh SSA
/// value (emitting an `InstrKind::Const` for literals). Use this
/// when the caller needs a `ValueId` to record in `locals`.
fn lower_expr_to_value(
    expr: &TypedExpr,
    b: &mut FunctionBuilder,
    locals: &mut Locals,
) -> Result<ValueId, LowerError> {
    let op = lower_expr_to_operand(expr, b, locals)?;
    Ok(materialize(op, expr.ty.clone(), expr.span, b))
}

/// Lower an expression to a possibly-constant operand. Used at
/// instruction operand position where the cheaper `Const` form
/// can flow through without a fresh SSA name.
fn lower_expr_to_operand(
    expr: &TypedExpr,
    b: &mut FunctionBuilder,
    locals: &mut Locals,
) -> Result<Operand, LowerError> {
    match &expr.kind {
        TypedExprKind::Int(v) => Ok(Operand::Const(Const::Int(*v))),
        TypedExprKind::Bool(v) => Ok(Operand::Const(Const::Bool(*v))),
        TypedExprKind::Float(v) => Ok(Operand::Const(Const::Float(*v))),
        TypedExprKind::Var(name) => match locals.get(name) {
            Some(v) => Ok(Operand::Value(*v)),
            None => Err(LowerError {
                message: format!("undeclared binding '{}'", name),
                span: expr.span,
            }),
        },
        TypedExprKind::Unary { op, expr: inner } => {
            let x = lower_expr_to_operand(inner, b, locals)?;
            let v = b.emit(
                expr.ty.clone(),
                expr.span,
                InstrKind::Unary { op: *op, x },
            );
            Ok(Operand::Value(v))
        }
        TypedExprKind::Binary { op, left, right, .. } => {
            let l = lower_expr_to_operand(left, b, locals)?;
            let r = lower_expr_to_operand(right, b, locals)?;
            // Str/OwnedStr `+` concat: tree backends lower
            // this to a runtime
            // `intent_str_concat(l, l_owned, r, r_owned)`
            // call. Each operand's `_owned` flag is 1 iff its
            // type is OwnedStr (the concat helper frees the
            // operand buffer when the flag is set).
            let is_str_concat = matches!(*op, crate::ast::BinaryOp::Add)
                && matches!(left.ty, Type::Str | Type::OwnedStr)
                && matches!(right.ty, Type::Str | Type::OwnedStr);
            if is_str_concat {
                let l_owned: i128 = if matches!(left.ty, Type::OwnedStr) { 1 } else { 0 };
                let r_owned: i128 = if matches!(right.ty, Type::OwnedStr) { 1 } else { 0 };
                let v = b.emit(
                    expr.ty.clone(),
                    expr.span,
                    InstrKind::Call {
                        name: "intent_str_concat".to_string(),
                        args: vec![
                            l,
                            Operand::Const(Const::Int(l_owned)),
                            r,
                            Operand::Const(Const::Int(r_owned)),
                        ],
                    },
                );
                return Ok(Operand::Value(v));
            }
            // Str comparison: tree backends lower this to
            // `strcmp(a, b) <op> 0`. Emit a synthetic
            // `intent_str_cmp(a, b) -> i64` call and then the
            // ordering compare against zero.
            let is_str_cmp = matches!(left.ty, Type::Str | Type::OwnedStr)
                && matches!(
                    op,
                    crate::ast::BinaryOp::Eq
                        | crate::ast::BinaryOp::Ne
                        | crate::ast::BinaryOp::Lt
                        | crate::ast::BinaryOp::Le
                        | crate::ast::BinaryOp::Gt
                        | crate::ast::BinaryOp::Ge
                );
            if is_str_cmp {
                // Fresh-OwnedStr operands (Call / Binary `+`
                // returning OwnedStr) own a heap allocation
                // with no other binding. `intent_str_cmp` is
                // just `strcmp`; it doesn't free anything.
                // Track which operands need a Drop after the
                // comparison and emit it. Closure #138 mirrors
                // closure #135's print whitelist.
                let l_needs_drop = crate::ir::is_fresh_owned_str(left);
                let r_needs_drop = crate::ir::is_fresh_owned_str(right);
                let l_for_drop = l.clone();
                let r_for_drop = r.clone();
                let cmp = b.emit(
                    Type::I64,
                    expr.span,
                    InstrKind::Call {
                        name: "intent_str_cmp".to_string(),
                        args: vec![l, r],
                    },
                );
                if l_needs_drop {
                    b.emit(
                        Type::I64,
                        expr.span,
                        InstrKind::Drop {
                            source: l_for_drop,
                            name: "_".to_string(),
                            ty: Type::OwnedStr,
                        },
                    );
                }
                if r_needs_drop {
                    b.emit(
                        Type::I64,
                        expr.span,
                        InstrKind::Drop {
                            source: r_for_drop,
                            name: "_".to_string(),
                            ty: Type::OwnedStr,
                        },
                    );
                }
                let v = b.emit(
                    expr.ty.clone(),
                    expr.span,
                    InstrKind::Binary {
                        op: *op,
                        l: Operand::Value(cmp),
                        r: Operand::Const(Const::Int(0)),
                    },
                );
                return Ok(Operand::Value(v));
            }
            let v = b.emit(
                expr.ty.clone(),
                expr.span,
                InstrKind::Binary { op: *op, l, r },
            );
            Ok(Operand::Value(v))
        }
        TypedExprKind::Call { name, args, .. } => {
            let lowered: Result<Vec<Operand>, LowerError> = args
                .iter()
                .map(|a| lower_expr_to_operand(a, b, locals))
                .collect();
            let v = b.emit(
                expr.ty.clone(),
                expr.span,
                InstrKind::Call {
                    name: name.clone(),
                    args: lowered?,
                },
            );
            Ok(Operand::Value(v))
        }
        TypedExprKind::Cast { expr: inner, ty } => {
            let x = lower_expr_to_operand(inner, b, locals)?;
            let v = b.emit(
                expr.ty.clone(),
                expr.span,
                InstrKind::Cast {
                    x,
                    to: ty.clone(),
                },
            );
            Ok(Operand::Value(v))
        }
        TypedExprKind::Str(s) => {
            let v = b.emit(
                expr.ty.clone(),
                expr.span,
                InstrKind::StrLit(s.clone()),
            );
            Ok(Operand::Value(v))
        }
        TypedExprKind::ArrayLit { elements } => {
            let lowered: Result<Vec<Operand>, LowerError> = elements
                .iter()
                .map(|e| lower_expr_to_operand(e, b, locals))
                .collect();
            let v = b.emit(
                expr.ty.clone(),
                expr.span,
                InstrKind::ArrayLit { elements: lowered? },
            );
            Ok(Operand::Value(v))
        }
        TypedExprKind::Index { array, index, checked } => {
            let a = lower_expr_to_operand(array, b, locals)?;
            let i = lower_expr_to_operand(index, b, locals)?;
            // Fresh-Vec operand (Call / Binary / Block /
            // IfExpr / Match returning Vec): the Index
            // instruction reads one element but doesn't free
            // the buffer. Without a Drop the heap leaks.
            // Var / FieldAccess Vec operands skip — the
            // binding's scope-exit Drop owns the buffer.
            // Closure #142.
            let needs_vec_drop = crate::ir::is_fresh_non_copy(array)
                && matches!(array.ty, Type::Vec(_));
            let a_for_drop = a.clone();
            let v = b.emit(
                expr.ty.clone(),
                expr.span,
                InstrKind::Index {
                    array: a,
                    index: i,
                    checked: *checked,
                },
            );
            if needs_vec_drop {
                b.emit(
                    Type::I64,
                    expr.span,
                    InstrKind::Drop {
                        source: a_for_drop,
                        name: "_".to_string(),
                        ty: array.ty.clone(),
                    },
                );
            }
            Ok(Operand::Value(v))
        }
        TypedExprKind::Len { array, length } => {
            let a = lower_expr_to_operand(array, b, locals)?;
            // `len(Str)` lowers to a runtime `strlen` call;
            // tree backends do the same. Arrays + Vecs reuse
            // the `InstrKind::Len` path because the length is
            // either a compile-time constant or a single
            // aggregate-field extract.
            if matches!(array.ty, Type::Str | Type::OwnedStr) {
                // Fresh-OwnedStr operand (Call / Binary `+`)
                // owns a heap allocation with no other
                // binding. `intent_str_len` (strlen) doesn't
                // consume its argument — emit a `Drop` after
                // the call. Var / FieldAccess / TupleAccess
                // operands skip the drop (binding owns).
                // Closure #139 mirrors #135 / #137 / #138.
                let needs_drop = crate::ir::is_fresh_owned_str(array);
                let a_for_drop = a.clone();
                let v = b.emit(
                    expr.ty.clone(),
                    expr.span,
                    InstrKind::Call {
                        name: "intent_str_len".to_string(),
                        args: vec![a],
                    },
                );
                if needs_drop {
                    b.emit(
                        Type::I64,
                        expr.span,
                        InstrKind::Drop {
                            source: a_for_drop,
                            name: "_".to_string(),
                            ty: Type::OwnedStr,
                        },
                    );
                }
                return Ok(Operand::Value(v));
            }
            // Fresh-Vec operand (Call returning Vec, Block /
            // IfExpr / Match returning Vec): the Len
            // instruction reads `.len` from the struct but
            // doesn't free the buffer; without a Drop the
            // heap leaks. Var / FieldAccess Vec operands skip
            // — the binding's scope-exit Drop owns the buffer.
            // Closure #141.
            let needs_vec_drop = crate::ir::is_fresh_non_copy(array)
                && matches!(array.ty, Type::Vec(_));
            let a_for_drop = a.clone();
            let v = b.emit(
                expr.ty.clone(),
                expr.span,
                InstrKind::Len {
                    array: a,
                    length: *length,
                },
            );
            if needs_vec_drop {
                b.emit(
                    Type::I64,
                    expr.span,
                    InstrKind::Drop {
                        source: a_for_drop,
                        name: "_".to_string(),
                        ty: array.ty.clone(),
                    },
                );
            }
            Ok(Operand::Value(v))
        }
        TypedExprKind::Ref { name } => {
            let source = locals
                .get(name)
                .map(|v| Operand::Value(*v))
                .ok_or_else(|| LowerError {
                    message: format!(
                        "Ref target '{}' has no SSA value (unknown binding)",
                        name
                    ),
                    span: expr.span,
                })?;
            let v = b.emit(
                expr.ty.clone(),
                expr.span,
                InstrKind::RefOf {
                    source,
                    mut_: false,
                },
            );
            Ok(Operand::Value(v))
        }
        TypedExprKind::RefMut { name } => {
            let source = locals
                .get(name)
                .map(|v| Operand::Value(*v))
                .ok_or_else(|| LowerError {
                    message: format!(
                        "RefMut target '{}' has no SSA value (unknown binding)",
                        name
                    ),
                    span: expr.span,
                })?;
            let v = b.emit(
                expr.ty.clone(),
                expr.span,
                InstrKind::RefOf {
                    source,
                    mut_: true,
                },
            );
            Ok(Operand::Value(v))
        }
        TypedExprKind::RefField { .. } | TypedExprKind::RefMutField { .. } => {
            // SSA path doesn't lower struct field-borrows yet —
            // surface a LowerError so the tree backend handles
            // the program. T1.2 phase 2b follow-up.
            Err(LowerError {
                message: "SSA lowering of struct field-borrows \
                          (`ref t.x` / `mut ref t.x`) is not yet \
                          implemented; routing to the tree backend"
                    .to_string(),
                span: expr.span,
            })
        }
        TypedExprKind::FnRef { name, .. } => {
            // Materialize the function pointer as an SSA value
            // of fn-ptr type. Backends consuming SSA emit the
            // matching symbol (`@fn_<name>` in LLVM, the bare
            // declarator-prefixed identifier in C).
            let v = b.emit(
                expr.ty.clone(),
                expr.span,
                InstrKind::FnRef { name: name.clone() },
            );
            Ok(Operand::Value(v))
        }
        TypedExprKind::CallIndirect { callee, args } => {
            // Lower the callee first (yields an SSA operand of
            // fn-ptr type), then each argument; emit a
            // dedicated CallIndirect instruction so analyses
            // can distinguish indirect calls from direct ones.
            let callee_op = lower_expr_to_operand(callee, b, locals)?;
            let mut arg_ops = Vec::with_capacity(args.len());
            for arg in args {
                arg_ops.push(lower_expr_to_operand(arg, b, locals)?);
            }
            let v = b.emit(
                expr.ty.clone(),
                expr.span,
                InstrKind::CallIndirect {
                    callee: callee_op,
                    args: arg_ops,
                },
            );
            Ok(Operand::Value(v))
        }
        TypedExprKind::Tuple { .. } | TypedExprKind::TupleAccess { .. } => {
            // Tuples aren't yet lowered through SSA — the
            // tree backends handle them directly. Surfacing
            // LowerError sends the program through the tree
            // fallback in `emit_c_via_ssa` /
            // `emit_llvm_via_ssa`. T1.1 follow-up wires SSA
            // lowering alongside `InstrKind::Tuple` /
            // `InstrKind::TupleAccess`.
            Err(LowerError {
                message: "SSA lowering of tuples is not yet supported".to_string(),
                span: expr.span,
            })
        }
        TypedExprKind::StructLit { .. } | TypedExprKind::FieldAccess { .. } => {
            // Same as tuples — tree backends own struct
            // emit for now; SSA support is T1.2 follow-up.
            Err(LowerError {
                message: "SSA lowering of structs is not yet supported".to_string(),
                span: expr.span,
            })
        }
        TypedExprKind::EnumVariant { .. }
        | TypedExprKind::EnumVariantWithPayload { .. }
        | TypedExprKind::Match { .. } => {
            Err(LowerError {
                message: "SSA lowering of enums / match is not yet supported".to_string(),
                span: expr.span,
            })
        }
        TypedExprKind::IfExpr { .. } => Err(LowerError {
            message: "SSA lowering of if-expressions is not yet supported".to_string(),
            span: expr.span,
        }),
        TypedExprKind::Block { .. } => Err(LowerError {
            message: "SSA lowering of block expressions is not yet supported".to_string(),
            span: expr.span,
        }),
    }
}

#[allow(dead_code)]
fn expr_kind_name(kind: &TypedExprKind) -> &'static str {
    match kind {
        TypedExprKind::Int(_) => "Int",
        TypedExprKind::Float(_) => "Float",
        TypedExprKind::Bool(_) => "Bool",
        TypedExprKind::Str(_) => "Str",
        TypedExprKind::Var(_) => "Var",
        TypedExprKind::Unary { .. } => "Unary",
        TypedExprKind::Binary { .. } => "Binary",
        TypedExprKind::Call { .. } => "Call",
        TypedExprKind::Cast { .. } => "Cast",
        TypedExprKind::ArrayLit { .. } => "ArrayLit",
        TypedExprKind::Index { .. } => "Index",
        TypedExprKind::Len { .. } => "Len",
        TypedExprKind::Ref { .. } => "Ref",
        TypedExprKind::RefMut { .. } => "RefMut",
        TypedExprKind::RefField { .. } => "RefField",
        TypedExprKind::RefMutField { .. } => "RefMutField",
        TypedExprKind::FnRef { .. } => "FnRef",
        TypedExprKind::CallIndirect { .. } => "CallIndirect",
        TypedExprKind::Tuple { .. } => "Tuple",
        TypedExprKind::TupleAccess { .. } => "TupleAccess",
        TypedExprKind::StructLit { .. } => "StructLit",
        TypedExprKind::FieldAccess { .. } => "FieldAccess",
        TypedExprKind::EnumVariant { .. } => "EnumVariant",
        TypedExprKind::EnumVariantWithPayload { .. } => "EnumVariantWithPayload",
        TypedExprKind::Match { .. } => "Match",
        TypedExprKind::IfExpr { .. } => "IfExpr",
        TypedExprKind::Block { .. } => "Block",
    }
}

fn materialize(
    operand: Operand,
    ty: Type,
    span: Span,
    b: &mut FunctionBuilder,
) -> ValueId {
    match operand {
        Operand::Value(v) => v,
        Operand::Const(c) => b.emit(ty, span, InstrKind::Const(c)),
    }
}

/// Tracks per-function SSA construction state. Bindings move
/// through `Locals` (a name→ValueId map) external to the builder
/// — the builder itself only knows about values, blocks, and
/// instructions.
struct FunctionBuilder {
    name: String,
    return_type: Type,
    params: Vec<(String, Type, ValueId)>,
    entry: BlockId,
    blocks: Vec<BasicBlock>,
    /// Set of in-flight terminators; once a block's terminator is
    /// set, no more instructions can be pushed to it.
    terminators: Vec<Option<Terminator>>,
    /// Type of every SSA value (params + instruction results +
    /// block-arg results), indexed by `ValueId.0`.
    value_types: Vec<Type>,
    current: BlockId,
    next_value: u32,
    /// Stack of in-progress loop frames so `break` and
    /// `continue` route to the right header / exit blocks and
    /// pass the right loop-carried block args.
    loops: Vec<LoopFrame>,
}

impl FunctionBuilder {
    fn new(name: String, return_type: Type) -> Self {
        Self {
            name,
            return_type,
            params: Vec::new(),
            entry: BlockId(0),
            blocks: Vec::new(),
            terminators: Vec::new(),
            value_types: Vec::new(),
            current: BlockId(0),
            next_value: 0,
            loops: Vec::new(),
        }
    }

    fn fresh_value(&mut self) -> ValueId {
        let v = ValueId(self.next_value);
        self.next_value += 1;
        // Placeholder type; replaced in `add_block_param` /
        // `emit`. Indexing by id stays O(1).
        self.value_types.push(Type::I64);
        v
    }

    fn value_type(&self, v: ValueId) -> &Type {
        &self.value_types[v.0 as usize]
    }

    fn new_block(&mut self) -> BlockId {
        let id = BlockId(self.blocks.len() as u32);
        self.blocks.push(BasicBlock {
            id,
            params: Vec::new(),
            instructions: Vec::new(),
            terminator: Terminator::Unreachable, // placeholder
        });
        self.terminators.push(None);
        id
    }

    fn set_current(&mut self, id: BlockId) {
        self.current = id;
    }

    fn current_block(&self) -> BlockId {
        self.current
    }

    fn current_block_terminator(&self) -> Option<&Terminator> {
        self.terminators[self.current.0 as usize].as_ref()
    }

    fn add_block_param(&mut self, block: BlockId, value: ValueId, ty: Type) {
        self.value_types[value.0 as usize] = ty.clone();
        self.blocks[block.0 as usize].params.push((value, ty));
    }

    fn emit(&mut self, ty: Type, span: Span, kind: InstrKind) -> ValueId {
        let v = self.fresh_value();
        self.value_types[v.0 as usize] = ty.clone();
        let instr = Instruction {
            result: v,
            kind,
            ty,
            span,
        };
        self.blocks[self.current.0 as usize].instructions.push(instr);
        v
    }

    fn terminate(&mut self, term: Terminator) {
        let idx = self.current.0 as usize;
        if self.terminators[idx].is_some() {
            // Already terminated — silently drop. This happens
            // when both branches of an if returned, so the
            // caller falls through expecting to attach a Jump;
            // we just stay terminated.
            return;
        }
        self.terminators[idx] = Some(term);
    }

    fn build(mut self) -> Function {
        for (i, term) in self.terminators.iter().enumerate() {
            self.blocks[i].terminator = term
                .clone()
                .unwrap_or(Terminator::Unreachable);
        }
        Function {
            name: self.name,
            params: self.params,
            return_type: self.return_type,
            entry: self.entry,
            blocks: self.blocks,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile;

    fn lower_main(src: &str) -> Function {
        let checked = compile(src).expect("source compiles");
        let main = checked
            .ir
            .functions
            .iter()
            .find(|f| f.name == "main")
            .expect("main exists");
        lower_function(main).expect("lower succeeds")
    }

    #[test]
    fn straight_line_function_has_one_block_terminated_by_return() {
        // The checker rewrites `return 42` as
        //   let __intent_ret_X: i64 = 42;
        //   return __intent_ret_X;
        // so the SSA shape is: one Const instruction holding
        // the literal, terminator returns that value.
        let src = "fn main() -> i64 { return 42; }";
        let fnc = lower_main(src);
        assert_eq!(fnc.blocks.len(), 1, "expected a single block");
        let block = &fnc.blocks[0];
        assert_eq!(
            block.instructions.len(),
            1,
            "expected exactly one Const instruction, got: {:?}",
            block.instructions
        );
        assert!(matches!(
            block.instructions[0].kind,
            InstrKind::Const(Const::Int(42))
        ));
        match &block.terminator {
            Terminator::Return(Some(Operand::Value(v))) => {
                assert_eq!(*v, block.instructions[0].result);
            }
            other => panic!("expected return of the const value, got {:?}", other),
        }
    }

    #[test]
    fn let_threads_value_into_subsequent_use() {
        // `let x: i64 = 41` and the checker-introduced return
        // temp both materialize as Const instructions; the
        // `x + 1` lowers to one Binary instruction; the return
        // references the binary's result. Three instructions
        // total.
        let src = "fn main() -> i64 { let x: i64 = 41; return x + 1; }";
        let fnc = lower_main(src);
        assert_eq!(fnc.blocks.len(), 1);
        let block = &fnc.blocks[0];
        assert_eq!(
            block.instructions.len(),
            2,
            "expected const-41 + binary-add, got: {:?}",
            block.instructions
        );
        assert!(matches!(
            block.instructions[0].kind,
            InstrKind::Const(Const::Int(41))
        ));
        assert!(matches!(
            block.instructions[1].kind,
            InstrKind::Binary { op: BinaryOp::Add, .. }
        ));
        match &block.terminator {
            Terminator::Return(Some(Operand::Value(v))) => {
                assert_eq!(
                    *v, block.instructions[1].result,
                    "return should reference the binary's result"
                );
            }
            other => panic!("expected return %v, got {:?}", other),
        }
    }

    #[test]
    fn ssa_values_are_unique_and_sequential() {
        let src = "fn main() -> i64 { let a: i64 = 1; let b: i64 = 2; return a + b; }";
        let fnc = lower_main(src);
        // No params, two emitted values (one const each for
        // %a and %b, then one binary), and the merge isn't
        // involved — so values are 0, 1, 2.
        // Let me just check uniqueness rather than exact count.
        let mut seen = std::collections::HashSet::new();
        for block in &fnc.blocks {
            for instr in &block.instructions {
                assert!(
                    seen.insert(instr.result.0),
                    "duplicate SSA value %{}",
                    instr.result.0
                );
            }
            for (v, _) in &block.params {
                assert!(seen.insert(v.0), "duplicate block param %{}", v.0);
            }
        }
    }

    #[test]
    fn if_else_with_no_reassign_has_no_merge_params() {
        let src = r#"
            fn main() -> i64 {
              if 1 < 2 { return 1; } else { return 0; }
            }
        "#;
        let fnc = lower_main(src);
        // entry, then, else — no merge block because both
        // branches terminate.
        assert!(
            fnc.blocks.len() >= 3,
            "expected at least 3 blocks, got {}: {}",
            fnc.blocks.len(),
            fnc
        );
        // No block should have parameters because no merge
        // needed any (and entry has no params for nullary
        // main).
        for block in &fnc.blocks {
            assert!(
                block.params.is_empty(),
                "unexpected params on bb{}: {:?}",
                block.id.0,
                block.params
            );
        }
    }

    #[test]
    fn if_with_falling_through_then_creates_merge_block_with_params_for_modified_var() {
        // `x` is modified in the then-branch; the merge block
        // must take a single i64 parameter for it.
        let src = r#"
            fn main() -> i64 {
              let x: i64 = 0;
              if 1 < 2 { x = 5; }
              return x;
            }
        "#;
        let fnc = lower_main(src);
        // Find the merge block: the one with at least one
        // parameter named after `x`.
        let merge = fnc
            .blocks
            .iter()
            .find(|b| !b.params.is_empty())
            .unwrap_or_else(|| panic!("expected a merge block with params, got:\n{}", fnc));
        assert_eq!(merge.params.len(), 1, "merge has one block-arg for x");
        assert!(
            matches!(merge.params[0].1, Type::I64),
            "merge param type should be i64"
        );
        // Return should reference the merge param.
        let returns = fnc
            .blocks
            .iter()
            .filter(|b| matches!(b.terminator, Terminator::Return(_)))
            .count();
        assert_eq!(returns, 1, "exactly one Return terminator");
    }

    #[test]
    fn function_parameters_become_block_params_of_entry() {
        let src = "fn inc(x: i64) -> i64 { return x + 1; }";
        let checked = compile(&format!("{}\nfn main() -> i64 {{ return 0; }}", src))
            .expect("compiles");
        let inc = checked
            .ir
            .functions
            .iter()
            .find(|f| f.name == "inc")
            .unwrap();
        let fnc = lower_function(inc).expect("lower succeeds");
        let entry = &fnc.blocks[fnc.entry.0 as usize];
        assert_eq!(entry.params.len(), 1);
        assert!(matches!(entry.params[0].1, Type::I64));
        // Function records the param mapping too.
        assert_eq!(fnc.params.len(), 1);
        assert_eq!(fnc.params[0].0, "x");
    }

    #[test]
    fn parallel_for_lowers_with_begin_end_hints() {
        let src = r#"
            fn main() -> i64 {
              parallel for i from 0 to 3 { let _ = i; }
              return 0;
            }
        "#;
        let fnc = lower_main(src);
        let has_begin = fnc
            .blocks
            .iter()
            .flat_map(|b| b.instructions.iter())
            .any(|i| matches!(i.kind, InstrKind::Hint(HintKind::ParallelForBegin { .. })));
        let has_end = fnc
            .blocks
            .iter()
            .flat_map(|b| b.instructions.iter())
            .any(|i| matches!(i.kind, InstrKind::Hint(HintKind::ParallelForEnd)));
        assert!(has_begin, "missing ParallelForBegin hint:\n{}", fnc);
        assert!(has_end, "missing ParallelForEnd hint:\n{}", fnc);
    }

    #[test]
    fn integer_for_lowers_to_header_body_exit_with_loop_var_block_arg() {
        // `for i in 0..3 { let _ = i; }` should produce a
        // header block that carries `i` as a block parameter
        // and a body that increments it. Counting:
        //   bb0: entry, jump to bb1(0)
        //   bb1(i): cmp i < 3 → br bb2 / bb3(i)
        //   bb2: body + i+1, jump back to bb1
        //   bb3: continuation (return temp + return)
        let src = r#"
            fn main() -> i64 {
              for i from 0 to 3 { let _ = i; }
              return 0;
            }
        "#;
        let fnc = lower_main(src);
        assert!(
            fnc.blocks.len() >= 4,
            "expected at least 4 blocks for a for-loop, got {}:\n{}",
            fnc.blocks.len(),
            fnc
        );
        // Find a block with exactly one i64 param — that's the
        // header (loop var).
        let header = fnc
            .blocks
            .iter()
            .find(|b| {
                b.params.len() == 1
                    && matches!(b.params[0].1, Type::I64)
                    && matches!(
                        b.terminator,
                        Terminator::Branch { .. }
                    )
            })
            .unwrap_or_else(|| panic!("expected a header block with i64 param, got:\n{}", fnc));
        // Body block: ends with a Jump back to header.
        let header_id = header.id;
        let back_jump = fnc
            .blocks
            .iter()
            .any(|b| matches!(&b.terminator, Terminator::Jump { target, .. } if *target == header_id));
        assert!(
            back_jump,
            "expected at least one back-jump to the header bb{}:\n{}",
            header_id.0,
            fnc
        );
    }

    #[test]
    fn while_loop_carries_a_modified_binding_as_a_block_arg() {
        // `n` is read + reassigned inside the body, so it
        // becomes a loop-carried block-arg on the header.
        let src = r#"
            fn main() -> i64 {
              let n: i64 = 0;
              while n < 5 {
                n = n + 1;
              }
              return n;
            }
        "#;
        let fnc = lower_main(src);
        // Find the header block — it should take a single i64
        // param (for `n`) and end with a conditional branch.
        let header = fnc
            .blocks
            .iter()
            .find(|b| {
                b.params.len() == 1
                    && matches!(b.params[0].1, Type::I64)
                    && matches!(b.terminator, Terminator::Branch { .. })
            })
            .unwrap_or_else(|| panic!("expected while header with i64 carry param:\n{}", fnc));
        // The exit block should also take the same i64 param so
        // the final value of `n` is observable in the return.
        if let Terminator::Branch { else_block, .. } = header.terminator {
            let exit = &fnc.blocks[else_block.0 as usize];
            assert_eq!(
                exit.params.len(),
                1,
                "exit should carry n's final value:\n{}",
                fnc
            );
        }
    }

    #[test]
    fn break_terminates_with_jump_to_loop_exit() {
        // `break` inside a while loop should produce a Jump
        // to the loop's exit block (forwarding loop-carried
        // values). The header still has its own conditional
        // branch as the "natural" exit edge.
        let src = r#"
            fn main() -> i64 {
              let n: i64 = 0;
              while true {
                if n > 2 { break; }
                n = n + 1;
              }
              return n;
            }
        "#;
        let fnc = lower_main(src);
        // Find the exit block — has a single i64 param.
        let exit = fnc
            .blocks
            .iter()
            .find(|b| b.params.len() == 1 && matches!(b.params[0].1, Type::I64))
            .unwrap_or_else(|| panic!("expected exit block with i64 carry:\n{}", fnc));
        let exit_id = exit.id;
        let break_jumps = fnc
            .blocks
            .iter()
            .filter(|b| matches!(&b.terminator, Terminator::Jump { target, .. } if *target == exit_id))
            .count();
        // At least two jumps target the exit: the break and
        // the header's "cond is false" else_block (wait,
        // header uses a Branch, not a Jump). So only the break
        // is a Jump to exit.
        assert!(
            break_jumps >= 1,
            "expected at least one break-jump to exit bb{}:\n{}",
            exit_id.0,
            fnc
        );
    }

    #[test]
    fn array_let_lowers_to_array_lit_instruction() {
        let src = r#"
            fn main() -> i64 {
              let xs: [i64; 3] = [10, 20, 30];
              return xs[1];
            }
        "#;
        let fnc = lower_main(src);
        let array_lit_count = fnc
            .blocks
            .iter()
            .flat_map(|b| b.instructions.iter())
            .filter(|i| matches!(i.kind, InstrKind::ArrayLit { .. }))
            .count();
        assert_eq!(array_lit_count, 1, "expected 1 ArrayLit:\n{}", fnc);
        let index_count = fnc
            .blocks
            .iter()
            .flat_map(|b| b.instructions.iter())
            .filter(|i| matches!(i.kind, InstrKind::Index { .. }))
            .count();
        assert_eq!(index_count, 1, "expected 1 Index:\n{}", fnc);
    }

    #[test]
    fn index_assign_lowers_to_index_assign_instruction() {
        let src = r#"
            fn main() -> i64 {
              let xs: [i64; 3] = [10, 20, 30];
              xs[0] = 99;
              return xs[0];
            }
        "#;
        let fnc = lower_main(src);
        let ia_count = fnc
            .blocks
            .iter()
            .flat_map(|b| b.instructions.iter())
            .filter(|i| matches!(i.kind, InstrKind::IndexAssign { .. }))
            .count();
        assert_eq!(ia_count, 1, "expected 1 IndexAssign:\n{}", fnc);
    }

    #[test]
    fn print_lowers_to_intent_print_call() {
        let src = r#"
            fn main() -> i64 {
              print 42;
              return 0;
            }
        "#;
        let fnc = lower_main(src);
        let item_calls = fnc
            .blocks
            .iter()
            .flat_map(|b| b.instructions.iter())
            .filter(|i| matches!(&i.kind, InstrKind::Call { name, .. } if name == "intent_print_item"))
            .count();
        let putc_calls = fnc
            .blocks
            .iter()
            .flat_map(|b| b.instructions.iter())
            .filter(|i| matches!(&i.kind, InstrKind::Call { name, .. } if name == "intent_print_putc"))
            .count();
        assert_eq!(item_calls, 1, "one intent_print_item per Expr item:\n{}", fnc);
        assert_eq!(
            putc_calls, 1,
            "one terminator intent_print_putc(10):\n{}",
            fnc
        );
    }

    #[test]
    fn for_iter_lowers_to_counter_loop_with_index_load() {
        let src = r#"
            fn main() -> i64 {
              let xs: [i64; 3] = [10, 20, 30];
              let s: i64 = 0;
              for x in ref xs {
                s = s + x;
              }
              return s;
            }
        "#;
        let fnc = lower_main(src);
        // The for-iter introduces an Index instruction (to
        // load xs[idx]) and a Len instruction (to compute the
        // upper bound).
        let len_count = fnc
            .blocks
            .iter()
            .flat_map(|b| b.instructions.iter())
            .filter(|i| matches!(i.kind, InstrKind::Len { .. }))
            .count();
        assert_eq!(len_count, 1, "expected 1 Len:\n{}", fnc);
    }

    #[test]
    fn ref_expr_lowers_to_ref_of_instruction() {
        let src = r#"
            pure fn first(xs: ref [i64; 3]) -> i64 { return xs[0]; }
            fn main() -> i64 {
              let xs: [i64; 3] = [10, 20, 30];
              return first(ref xs);
            }
        "#;
        let fnc = lower_main(src);
        let refs = fnc
            .blocks
            .iter()
            .flat_map(|b| b.instructions.iter())
            .filter(|i| matches!(i.kind, InstrKind::RefOf { .. }))
            .count();
        assert_eq!(refs, 1, "expected 1 RefOf:\n{}", fnc);
    }

    #[test]
    fn parallel_for_reduce_records_reduction_metadata_on_hint() {
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
        let fnc = lower_main(src);
        let begin_hint = fnc
            .blocks
            .iter()
            .flat_map(|b| b.instructions.iter())
            .find_map(|i| match &i.kind {
                InstrKind::Hint(HintKind::ParallelForBegin { reductions, .. }) => Some(reductions),
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected ParallelForBegin hint:\n{}", fnc));
        assert_eq!(begin_hint.len(), 1, "expected one reduction recorded");
        assert_eq!(begin_hint[0].0, "total");
        assert!(matches!(
            begin_hint[0].1,
            crate::ast::ReductionOp::Add
        ));
    }

    #[test]
    fn parallel_for_begin_carries_structured_loop_shape() {
        // The Hint::ParallelForBegin now packs a
        // `ParallelForShape` so backends can emit OpenMP /
        // GOMP without walking the CFG. The shape records
        // the counter name + type, the start/end operands,
        // and the header/body/exit BlockIds the lowerer
        // already produced for the sequential lowering.
        let src = r#"
            fn main() -> i64 {
              let total: i64 = 0;
              parallel for i from 0 to 8
              reduce total with +;
              {
                total = total + i;
              }
              return total;
            }
        "#;
        let fnc = lower_main(src);
        let shape = fnc
            .blocks
            .iter()
            .flat_map(|b| b.instructions.iter())
            .find_map(|i| match &i.kind {
                InstrKind::Hint(HintKind::ParallelForBegin { shape, .. }) => {
                    Some(shape.clone())
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("expected ParallelForBegin shape:\n{}", fnc));
        assert_eq!(shape.counter_name, "i");
        assert_eq!(shape.counter_ty, Type::I64);
        // Header / body / exit are distinct, non-default
        // BlockIds (the placeholder uses BlockId(0) which is
        // also the entry block — but the loop's header is
        // freshly allocated so its id will be > 0).
        assert!(
            shape.header_block.0 > 0,
            "header_block should be a fresh BlockId, got {:?}",
            shape.header_block
        );
        assert!(
            shape.body_block.0 > 0 && shape.body_block != shape.header_block,
            "body_block should be distinct from header_block: header={:?}, body={:?}",
            shape.header_block,
            shape.body_block
        );
        assert!(
            shape.exit_block.0 > 0
                && shape.exit_block != shape.header_block
                && shape.exit_block != shape.body_block,
            "exit_block should be distinct: header={:?}, body={:?}, exit={:?}",
            shape.header_block,
            shape.body_block,
            shape.exit_block
        );
        // start / end are operands the lowerer produced when
        // lowering the start/end exprs — could be a Const or
        // a Value depending on the expression. Just verify
        // they aren't the placeholder sentinel
        // (`Const(Int(0))` for both is the placeholder pair;
        // even if start happens to be Const(0), end will not
        // be Const(0) for a `0..8` loop).
        match (&shape.start, &shape.end) {
            (Operand::Const(Const::Int(0)), Operand::Const(Const::Int(0))) => {
                panic!(
                    "shape was not patched from placeholder (both start and end are Const(0)):\n{}",
                    fnc
                );
            }
            _ => {}
        }
        // The header value-id matches the SSA-level first
        // block param of the header block — i.e. it's a
        // value that's actually defined in the IR.
        assert!(
            shape.counter_header_value.0 > 0,
            "counter_header_value should be a real ValueId: {:?}",
            shape.counter_header_value
        );
    }

    #[test]
    fn task_spawn_and_join_lower_to_matching_hints() {
        // The body captures `x0` (a Copy scalar pre-extracted
        // from the array). The SSA lowerer emits matching
        // TaskBegin / TaskEnd / TaskJoin hints around the
        // body's instructions.
        let src = r#"
            fn main() -> i64 {
              let xs: [i64; 3] = [1, 2, 3];
              let x0: i64 = xs[0];
              task ta { let v: i64 = x0; let _ = v; }
              join ta;
              return 0;
            }
        "#;
        let fnc = lower_main(src);
        let begin = fnc
            .blocks
            .iter()
            .flat_map(|b| b.instructions.iter())
            .filter(|i| matches!(&i.kind, InstrKind::Hint(HintKind::TaskBegin { handle }) if handle == "ta"))
            .count();
        let end = fnc
            .blocks
            .iter()
            .flat_map(|b| b.instructions.iter())
            .filter(|i| matches!(&i.kind, InstrKind::Hint(HintKind::TaskEnd { handle }) if handle == "ta"))
            .count();
        let join = fnc
            .blocks
            .iter()
            .flat_map(|b| b.instructions.iter())
            .filter(|i| matches!(&i.kind, InstrKind::Hint(HintKind::TaskJoin { handle }) if handle == "ta"))
            .count();
        assert_eq!(begin, 1, "expected one TaskBegin hint:\n{}", fnc);
        assert_eq!(end, 1, "expected one TaskEnd hint:\n{}", fnc);
        assert_eq!(join, 1, "expected one TaskJoin hint:\n{}", fnc);
    }

    #[test]
    fn pretty_print_round_trips_to_a_useful_textual_form() {
        let src = "fn main() -> i64 { return 1 + 2; }";
        let fnc = lower_main(src);
        let text = format!("{}", fnc);
        // Sanity: text should mention `fn @main` and `return`.
        assert!(text.contains("fn @main"), "missing fn header:\n{}", text);
        assert!(text.contains("return"), "missing return:\n{}", text);
        assert!(text.contains("+"), "missing binary op:\n{}", text);
    }
}
