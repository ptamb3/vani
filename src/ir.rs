use crate::ast::{BinaryOp, ReductionOp, Type, UnaryOp};
use crate::span::Span;

/// IR mirror of `ast::Reduction`. Same shape; included so backends
/// can dispatch without depending on the AST module.
#[derive(Clone, Debug, PartialEq)]
pub struct TypedReduction {
    pub var: String,
    pub op: ReductionOp,
    pub ty: Type,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TypedProgram {
    pub intents: Vec<String>,
    pub functions: Vec<TypedFunction>,
    /// Validated struct declarations carried through to the
    /// backends so each can emit its per-struct C / LLVM
    /// type definition. Refines T1.2.
    pub structs: Vec<TypedStructDecl>,
    /// Validated enum declarations. T1.3.
    pub enums: Vec<TypedEnumDecl>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TypedStructDecl {
    pub name: String,
    pub fields: Vec<(String, Type)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TypedEnumDecl {
    pub name: String,
    /// Variant names in declaration order. The variant's
    /// integer tag is its position. T1.3.
    pub variants: Vec<String>,
    /// Per-variant payload type (parallel to `variants`).
    /// `None` for payload-less variants; `Some(ty)` for
    /// payloaded variants. v1 requires all payload-bearing
    /// variants to share the same payload type (the C
    /// backend lays it out as `typedef struct { int32_t tag;
    /// T payload; } Enum_<Name>;`). T1.3 phase 2b.
    pub payload_types: Vec<Option<Type>>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TypedFunction {
    pub name: String,
    pub params: Vec<TypedParam>,
    pub return_type: Type,
    pub requires: Vec<TypedExpr>,
    pub body: Vec<TypedStmt>,
    /// Set by the parser/checker when the function was declared
    /// `pure fn`. The checker has verified the body is
    /// side-effect-free; backends may use this for optimization
    /// (e.g., CSE across calls). Callers in a pure context or a
    /// `parallel for` body may only invoke pure functions.
    pub is_pure: bool,
    /// Source-byte range covering the entire `fn` declaration
    /// (`fn` keyword through the closing `}`). Carried forward
    /// from the AST so LSP features can pin "which function
    /// does the cursor belong to" without re-parsing.
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TypedParam {
    pub name: String,
    pub ty: Type,
    /// Span of the parameter's name identifier in source.
    /// Carried forward from the AST so LSP semantic tokens
    /// can mark each parameter declaration with the
    /// `parameter` type.
    pub name_span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TypedPrintItem {
    Expr(TypedExpr),
    Str(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum TypedStmt {
    Let {
        name: String,
        ty: Type,
        expr: TypedExpr,
    },
    Reassign {
        name: String,
        ty: Type,
        expr: TypedExpr,
        drop_old: bool,
    },
    Drop {
        name: String,
        ty: Type,
        /// For struct bindings: names of fields that have
        /// been moved out of this binding before scope exit
        /// (via `let y = t.f;` or `f(t.f)` patterns). The
        /// per-field free pass in both backends skips these
        /// to avoid freeing a value that another binding now
        /// owns. Empty for non-struct types and structs with
        /// no partial moves. T1.2 phase 2b partial-move
        /// follow-up.
        moved_fields: Vec<String>,
    },
    /// Evaluate `expr` for its side effects (or to consume an affine value)
    /// and discard the result. Produced by `let _ = expr;`.
    Discard {
        expr: TypedExpr,
    },
    Return {
        expr: TypedExpr,
    },
    Assert {
        expr: TypedExpr,
        message: Option<String>,
    },
    Prove {
        expr: TypedExpr,
    },
    Print {
        items: Vec<TypedPrintItem>,
    },
    If {
        cond: TypedExpr,
        then_body: Vec<TypedStmt>,
        else_body: Vec<TypedStmt>,
    },
    While {
        cond: TypedExpr,
        body: Vec<TypedStmt>,
    },
    Break,
    Continue,
    IndexAssign {
        name: String,
        /// The static type of the base binding being indexed (owned [T;N],
        /// owned Vec<T>, &mut [T;N], or &mut Vec<T>). The backend uses this
        /// to choose the correct C lowering form.
        base_ty: Type,
        index: TypedExpr,
        /// `xs[i].field = …;` — single-level field path on
        /// the indexed element. Each entry is (field_name,
        /// field_index). Empty for plain `xs[i] = v;`. v1
        /// supports single-level paths only. T1.2 phase 2b
        /// follow-up.
        field_path: Vec<(String, u32)>,
        value: TypedExpr,
        /// Whether to emit a runtime bounds check. Compile-time-discharged
        /// constant indices on owned arrays skip the check.
        checked: bool,
    },
    /// `obj.field = value;` — field assignment. The object
    /// is a typed place expression (Var or FieldAccess) and
    /// must be either an owned struct or a `mut ref` to one.
    /// T1.2 phase 2a follow-up.
    FieldAssign {
        object: TypedExpr,
        field: String,
        /// Numeric index of the field in the underlying
        /// struct (0-based, declaration order). Backends
        /// use this for `obj.field` access — C uses the
        /// field name; LLVM uses the index.
        field_index: u32,
        /// Whether the receiver was a `mut ref` (in which
        /// case the backend dereferences before assigning).
        through_mut_ref: bool,
        value: TypedExpr,
    },
    For {
        var: String,
        ty: Type,
        start: TypedExpr,
        end: TypedExpr,
        body: Vec<TypedStmt>,
        /// True when the source had `parallel for i in start..end`.
        /// The checker has verified the body has no side effects
        /// and only calls pure functions, so every iteration is
        /// independent. Backends today still lower this as a
        /// sequential for loop — semantics-preserving — leaving
        /// actual threading as a backend follow-up.
        parallel: bool,
        /// Reduction clauses (`reduce <var> with <op>;`) attached
        /// to the parallel form. The checker has verified each
        /// reduction variable is updated only via the declared op
        /// inside the body. Backends use this list to either pass
        /// `reduction(op:var)` to OpenMP (C) or rewrite the body's
        /// Reassign to `atomicrmw` (LLVM).
        reductions: Vec<TypedReduction>,
    },
    ForIter {
        var: String,
        /// Type of the element (Copy primitive).
        element_ty: Type,
        /// Collection binding name.
        collection: String,
        /// Type of the collection (owned [T;N] / Vec<T> or &/&mut variants).
        /// Backend dispatches on this to choose array decay vs Vec field
        /// access, and ref deref where needed.
        collection_ty: Type,
        /// True when the collection was consumed (`for x in xs`), as
        /// opposed to borrowed (`for x in &xs`). When true and the
        /// collection is `Vec<T>`, the backend frees the buffer after the
        /// loop body.
        consumes: bool,
        body: Vec<TypedStmt>,
    },
    /// `task <name> { <body> }` — declares an affine `Task` handle
    /// named `<name>`. The body has been verified pure-with-
    /// captures by the checker. `captures` is the ordered list
    /// of outer-scope bindings the body references, paired with
    /// their types; the backends lower the spawn to an outlined
    /// pthread function whose ctx struct holds these captures
    /// by value (captures are restricted to Copy types).
    TaskSpawn {
        name: String,
        body: Vec<TypedStmt>,
        captures: Vec<(String, Type)>,
    },
    /// `join <name>;` — consumes a previously-declared `Task`
    /// handle. The checker's affine tracking guarantees each
    /// spawn has exactly one matching join in the same block.
    TaskJoin {
        name: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct TypedExpr {
    pub kind: TypedExprKind,
    pub ty: Type,
    pub constant: Option<TypedConst>,
    pub span: Span,
    /// For binding references (`Var`, `Ref`, `RefMut`), the
    /// declaration site of the binding being referenced.
    /// Populated by the checker via env lookup; `None` for
    /// all other kinds and for synthetic / unresolvable
    /// references. LSP features (references, rename,
    /// completion) use this to distinguish two same-name
    /// bindings in different scopes — without it, the
    /// walkers fall back to name-only matching.
    pub binding_decl_span: Option<Span>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TypedExprKind {
    Int(i128),
    Float(f64),
    Bool(bool),
    Str(String),
    Var(String),
    Unary {
        op: UnaryOp,
        expr: Box<TypedExpr>,
    },
    Binary {
        op: BinaryOp,
        left: Box<TypedExpr>,
        right: Box<TypedExpr>,
        /// Whether the runtime safety guard (divisor != 0 for Div/Rem,
        /// 0 <= rhs < bits for Shl/Shr) is still required. Default
        /// true; the SMT-discharge pass flips to false when the
        /// guard is provably unnecessary. Ignored for Add/Sub/Mul/
        /// comparison ops.
        checked: bool,
    },
    Call {
        name: String,
        /// Span of just the callee identifier, mirrored from
        /// the AST. Defaults to the wrapping `TypedExpr.span`
        /// for synthetic calls (e.g. the
        /// `__intent_atomic_*` rewrites in the LLVM
        /// backend's reduction lowering); LSP features that
        /// need a precise callee span fall back to
        /// `TypedExpr.span` in that case.
        name_span: Span,
        args: Vec<TypedExpr>,
    },
    Cast {
        expr: Box<TypedExpr>,
        ty: Type,
    },
    ArrayLit {
        elements: Vec<TypedExpr>,
    },
    Index {
        array: Box<TypedExpr>,
        index: Box<TypedExpr>,
        checked: bool,
    },
    Len {
        array: Box<TypedExpr>,
        length: u64,
    },
    Ref {
        name: String,
    },
    RefMut {
        name: String,
    },
    /// Borrow of a struct field — `ref t.x` / `mut ref t.x`.
    /// `object` is the binding name; `field` and `field_index`
    /// identify the field. Result type is `Type::Ref(field_ty)`
    /// / `Type::RefMut(field_ty)` on the wrapper. v1 supports
    /// single-level field-borrow only (no `ref a.b.c`); deeper
    /// paths can be added when the use cases surface.
    /// T1.2 phase 2b follow-up.
    RefField {
        object: String,
        field: String,
        field_index: u32,
    },
    RefMutField {
        object: String,
        field: String,
        field_index: u32,
    },
    /// Reference to a top-level function as a first-class
    /// value. Produced when an identifier in value position
    /// resolves to a function (not a binding). The result type
    /// is `fn(T1, ...) -> R` matching the function's signature.
    /// Backends emit the function's address (`@name` in LLVM,
    /// the bare identifier in C — function names decay to
    /// pointers there).
    FnRef {
        name: String,
        name_span: Span,
    },
    /// Indirect call through a fn-ptr expression. Distinct from
    /// the named `Call` variant: the callee is a value, not a
    /// global symbol. The static call-graph analyses
    /// (locks_params propagation, purity) can't see through
    /// indirect calls — they conservatively assume the callee
    /// may do anything, so the checker rejects indirect calls
    /// inside contexts that need those guarantees.
    CallIndirect {
        callee: Box<TypedExpr>,
        args: Vec<TypedExpr>,
    },
    /// Tuple constructor — typed form of `ExprKind::Tuple`. The
    /// `TypedExpr` wrapper's `ty` carries the resulting
    /// `Type::Tuple(elements)` shape. T1.1.
    Tuple {
        elements: Vec<TypedExpr>,
    },
    /// Tuple field read — typed form of `ExprKind::TupleAccess`.
    /// The wrapper's `ty` is the element type at `index`. T1.1.
    TupleAccess {
        tuple: Box<TypedExpr>,
        index: u32,
    },
    /// Struct literal — typed form of `ExprKind::StructLit`.
    /// `fields` are in struct-declaration order so backends
    /// can emit `insertvalue` chains by position. T1.2.
    StructLit {
        type_name: String,
        fields: Vec<(String, TypedExpr)>,
    },
    /// Struct field read — typed form of
    /// `ExprKind::FieldAccess`. `field_index` is the
    /// declaration-order position of the field in its
    /// struct, looked up by the checker. The wrapper's
    /// `ty` is the field's type. T1.2.
    FieldAccess {
        object: Box<TypedExpr>,
        field: String,
        field_index: u32,
    },
    /// Enum variant reference — typed form of an enum
    /// `Color.Red` literal. `tag` is the variant's
    /// integer position (assigned in declaration order).
    /// T1.3.
    EnumVariant {
        enum_name: String,
        variant: String,
        tag: u32,
    },
    /// Enum constructor with a payload: `Opt.Some(42)`.
    /// V1 supports single-field payloads only. Backends lay
    /// the enum out as a tagged union (`{ i64 tag; union {
    /// .. } }` in C, `{i64, [N x i8]}` in LLVM). T1.3 phase
    /// 2b.
    EnumVariantWithPayload {
        enum_name: String,
        variant: String,
        tag: u32,
        payload: Box<TypedExpr>,
        payload_ty: Type,
    },
    /// Match expression. Arms carry their variant tag for
    /// backend dispatch. The wrapper's `ty` is the
    /// (unified) arm-body type. T1.3.
    Match {
        scrutinee: Box<TypedExpr>,
        arms: Vec<TypedMatchArm>,
    },
    /// `if cond { expr } else { expr }` as an expression.
    /// Both branches' types unify; the wrapper's `ty` is
    /// the unified branch type. T4 (if-as-expression).
    IfExpr {
        cond: Box<TypedExpr>,
        then_value: Box<TypedExpr>,
        else_value: Box<TypedExpr>,
    },
    /// `{ stmt; stmt; tail-expr }` — block expression.
    /// `stmts` execute in order; `tail`'s value becomes the
    /// block's value. The wrapper's `ty` matches `tail.ty`.
    /// Scope handling already exists via the lower_if-style
    /// shadow-restoration in SSA lowering. T-block.
    Block {
        stmts: Vec<TypedStmt>,
        tail: Box<TypedExpr>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct TypedMatchArm {
    pub variant: String,
    pub tag: u32,
    /// True when this arm matches `_ then …` — covers every
    /// remaining variant. Backends emit it as the
    /// `default` case of their dispatch. T1.3 (wildcard
    /// addition).
    pub is_wildcard: bool,
    /// Integer-literal pattern value, when this arm is
    /// dispatching on a scrutinee of integer type rather
    /// than an enum tag. Backends use this in their switch
    /// `case` label. None for variant + wildcard arms.
    /// T1.3 integer-literal pattern.
    pub int_value: Option<i128>,
    /// Optional payload binding name + its type for
    /// `Opt.Some(v) then …` destructure patterns. Backends
    /// emit `<payload_ty> v_<binding> = (scrutinee).payload;`
    /// at the start of the arm body so the body's reference
    /// to `v` resolves. None for non-binding patterns.
    /// T1.3 phase 2b.
    pub binding: Option<(String, Type)>,
    pub body: TypedExpr,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TypedConst {
    Int(i128),
    Float(f64),
    Bool(bool),
}

/// True when `expr` is an OwnedStr expression whose value
/// is a FRESH heap allocation with no other binding holding
/// it. Used at sites that READ an OwnedStr value without
/// consuming it (print, strcmp, strlen, match scrutinee) to
/// decide whether a `free` is needed after the read.
///
/// The conservative set: Call (user fn / intent_str_concat
/// produces a fresh heap), Binary `+` concat (same), Block
/// (its tail moves an inner let or evaluates to a fresh
/// expression; the block doesn't emit Drops for its inner
/// non-Copy lets so the value escapes to the outer context),
/// IfExpr (branches produce fresh values; the if-expr yields
/// one of them), Match (arm bodies produce fresh values).
///
/// The set excludes Var / FieldAccess / TupleAccess —
/// reading from those references a binding-owned heap, and
/// emitting a free at the use site would double-free at the
/// owner's scope-exit Drop. Closure #140 unified the
/// whitelist used by closures #135 / #137 / #138 / #139 into
/// a single helper and broadened it to cover Block / IfExpr
/// / Match (closure #139 surfaced the Block-len leak).
pub fn is_fresh_owned_str(expr: &TypedExpr) -> bool {
    if !matches!(expr.ty, Type::OwnedStr) {
        return false;
    }
    matches!(
        expr.kind,
        TypedExprKind::Call { .. }
            | TypedExprKind::Binary { .. }
            | TypedExprKind::Block { .. }
            | TypedExprKind::IfExpr { .. }
            | TypedExprKind::Match { .. }
    )
}
