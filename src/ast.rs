use crate::span::Span;
use std::fmt;

thread_local! {
    /// Names of structs declared with at least one non-Copy field
    /// (in v1, that means an `OwnedStr` field — other affine field
    /// types are still rejected at struct-decl time). Populated by
    /// the checker before any `Type::is_copy()` calls fire so that
    /// `Type::Struct(name)` correctly reports `false` for affine
    /// aggregates. Backends emit per-field free calls when one of
    /// these structs is dropped.
    pub(crate) static STRUCT_NON_COPY_REGISTRY: std::cell::RefCell<std::collections::HashSet<String>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
}

/// Reset and repopulate the non-Copy-struct registry. Called once
/// per `check_program` after the struct registry is built.
pub fn set_non_copy_structs<I: IntoIterator<Item = String>>(names: I) {
    STRUCT_NON_COPY_REGISTRY.with(|cell| {
        let mut set = cell.borrow_mut();
        set.clear();
        set.extend(names);
    });
}

/// True when `name` was registered as a struct with non-Copy
/// fields. Consulted by `Type::is_copy()` for `Type::Struct`.
pub fn struct_has_non_copy_field(name: &str) -> bool {
    STRUCT_NON_COPY_REGISTRY.with(|cell| cell.borrow().contains(name))
}

thread_local! {
    /// Names of enums declared with at least one non-Copy
    /// payload type (in v1, that means an `OwnedStr` payload).
    /// Populated by the checker so `Type::Enum(name)` reports
    /// `false` from `is_copy()` for affine aggregates.
    /// Backends emit per-variant free calls when one of these
    /// enums is dropped. T1.3 + T1.2 phase 2b.
    pub(crate) static ENUM_NON_COPY_REGISTRY: std::cell::RefCell<std::collections::HashSet<String>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
}

pub fn set_non_copy_enums<I: IntoIterator<Item = String>>(names: I) {
    ENUM_NON_COPY_REGISTRY.with(|cell| {
        let mut set = cell.borrow_mut();
        set.clear();
        set.extend(names);
    });
}

pub fn enum_has_non_copy_payload(name: &str) -> bool {
    ENUM_NON_COPY_REGISTRY.with(|cell| cell.borrow().contains(name))
}

thread_local! {
    /// (interface_name, concrete_type_name) pairs that have an
    /// `implement Iface for T { … }` declared. Populated by the
    /// checker before any `dyn Iface` coercion check fires.
    /// Consulted by `coerce_checked` to validate
    /// `let d: dyn Iface = t;`-style assignments. Epic A
    /// Phase 2 (closure #221).
    pub(crate) static IFACE_IMPL_REGISTRY: std::cell::RefCell<std::collections::HashSet<(String, String)>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
}

pub fn set_iface_impls<I: IntoIterator<Item = (String, String)>>(pairs: I) {
    IFACE_IMPL_REGISTRY.with(|cell| {
        let mut set = cell.borrow_mut();
        set.clear();
        set.extend(pairs);
    });
}

pub fn iface_impl_exists(iface: &str, type_name: &str) -> bool {
    IFACE_IMPL_REGISTRY.with(|cell| {
        cell.borrow().contains(&(iface.to_string(), type_name.to_string()))
    })
}

thread_local! {
    /// Per-interface method signatures: iface_name →
    /// Vec<(method_name, param_tys_including_self, return_ty)>.
    /// Populated by the checker before any `obj.method()`
    /// dispatch on a `dyn Iface` value fires. Phase 2b
    /// dispatch consults this to validate the call site and
    /// resolve its return type; codegen Phase 3 will reuse
    /// the same registry to lay out vtable slots in
    /// declaration order.
    pub(crate) static IFACE_METHOD_REGISTRY: std::cell::RefCell<std::collections::HashMap<String, Vec<(String, Vec<Type>, Type)>>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

pub fn set_iface_methods<I: IntoIterator<Item = (String, Vec<(String, Vec<Type>, Type)>)>>(entries: I) {
    IFACE_METHOD_REGISTRY.with(|cell| {
        let mut map = cell.borrow_mut();
        map.clear();
        for (iface, methods) in entries {
            map.insert(iface, methods);
        }
    });
}

pub fn iface_method_lookup(iface: &str, method: &str) -> Option<(usize, Vec<Type>, Type)> {
    IFACE_METHOD_REGISTRY.with(|cell| {
        let map = cell.borrow();
        let methods = map.get(iface)?;
        let (idx, (_, params, ret)) = methods
            .iter()
            .enumerate()
            .find(|(_, (n, _, _))| n == method)?;
        Some((idx, params.clone(), ret.clone()))
    })
}

thread_local! {
    /// Closure #243: registry of private module items, keyed
    /// by mangled name (`<mod>__priv__<item>`) → source-form
    /// path (`<mod>::<item>`). Populated by
    /// `flatten_modules_in_program`. Consulted by the
    /// checker's unknown-name diagnostic to surface a clearer
    /// "private item 'mod::item'" message when the user tried
    /// to reach a private item from outside its module.
    pub(crate) static PRIVATE_MODULE_ITEMS: std::cell::RefCell<std::collections::HashMap<String, String>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

pub fn set_private_module_items<I: IntoIterator<Item = (String, String)>>(entries: I) {
    PRIVATE_MODULE_ITEMS.with(|cell| {
        let mut map = cell.borrow_mut();
        map.clear();
        for (mangled, source_path) in entries {
            map.insert(mangled, source_path);
        }
    });
}

/// Given a user-facing path like `math::double` that the parser
/// converted to `math__double` but doesn't exist in the
/// signature/type tables, check whether the same name with the
/// private-item separator (`math__priv__double`) is registered.
/// Returns the source-form path if so. Used by the
/// unknown-name diagnostic in the checker.
pub fn lookup_private_item(parser_form_mangled: &str) -> Option<String> {
    // The parser-form mangled name has at most one `__` between
    // module and item (no extra `priv` segment). If the name
    // contains `__`, split into mod and item and look up the
    // private form.
    let parts: Vec<&str> = parser_form_mangled.splitn(2, "__").collect();
    if parts.len() != 2 {
        return None;
    }
    let priv_mangled = format!("{}__priv__{}", parts[0], parts[1]);
    PRIVATE_MODULE_ITEMS.with(|cell| cell.borrow().get(&priv_mangled).cloned())
}

pub fn iface_methods_for(iface: &str) -> Option<Vec<(String, Vec<Type>, Type)>> {
    IFACE_METHOD_REGISTRY.with(|cell| cell.borrow().get(iface).cloned())
}

pub fn all_iface_names() -> Vec<String> {
    IFACE_METHOD_REGISTRY.with(|cell| {
        let mut names: Vec<String> = cell.borrow().keys().cloned().collect();
        names.sort();
        names
    })
}

thread_local! {
    /// Per-interface impl list in declaration order: iface_name
    /// → Vec<type_name>. Populated alongside the impl pair set
    /// so Phase 3 codegen can iterate (iface, type) pairs in
    /// stable order without HashSet noise.
    pub(crate) static IFACE_IMPLS_LIST: std::cell::RefCell<std::collections::HashMap<String, Vec<String>>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

pub fn set_iface_impls_list<I: IntoIterator<Item = (String, Vec<String>)>>(entries: I) {
    IFACE_IMPLS_LIST.with(|cell| {
        let mut map = cell.borrow_mut();
        map.clear();
        for (iface, types) in entries {
            map.insert(iface, types);
        }
    });
}

pub fn impls_for_iface(iface: &str) -> Vec<String> {
    IFACE_IMPLS_LIST.with(|cell| cell.borrow().get(iface).cloned().unwrap_or_default())
}

thread_local! {
    /// Type names whose `implement Drop for T` declares
    /// `fn drop(self: mut ref T) -> i64` (epic C). The
    /// by-ref form lets the user run cleanup BEFORE the
    /// auto-emitted per-field drop pass; the value form
    /// (default) suppresses per-field drops to avoid the
    /// double-free pattern. Populated alongside the rest
    /// of the impl registries.
    pub(crate) static USER_DROP_BY_REF: std::cell::RefCell<std::collections::HashSet<String>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
}

pub fn set_user_drop_by_ref<I: IntoIterator<Item = String>>(names: I) {
    USER_DROP_BY_REF.with(|cell| {
        let mut set = cell.borrow_mut();
        set.clear();
        set.extend(names);
    });
}

pub fn user_drop_is_by_ref(type_name: &str) -> bool {
    USER_DROP_BY_REF.with(|cell| cell.borrow().contains(type_name))
}

pub fn ifaces_implemented_by(type_name: &str) -> std::collections::HashSet<String> {
    IFACE_IMPL_REGISTRY.with(|cell| {
        cell.borrow()
            .iter()
            .filter_map(|(iface, ty)| {
                if ty == type_name { Some(iface.clone()) } else { None }
            })
            .collect()
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct Program {
    pub intents: Vec<Intent>,
    pub functions: Vec<Function>,
    pub uses: Vec<Use>,
    /// User-declared record types. Order is declaration order
    /// (matters for codegen so a struct's fields can reference
    /// previously-declared structs). Refines T1.2.
    pub structs: Vec<StructDecl>,
    /// User-declared enum types. T1.3.
    pub enums: Vec<EnumDecl>,
    /// User-declared interfaces. T1.5.
    pub interfaces: Vec<InterfaceDecl>,
    /// `implement <Iface> for <Type> { … }` blocks. T1.5.
    pub impls: Vec<ImplDecl>,
    /// Top-level `const NAME: T = expr;` declarations. v1
    /// only accepts literal initializers + Copy types. T4.15.
    pub consts: Vec<ConstDecl>,
    /// Top-level `type Name = Type;` aliases. Resolved at
    /// check time — backends never see the alias name.
    /// T4.15 (alias half).
    pub type_aliases: Vec<TypeAlias>,
    /// `methods on TypeName { fn … { … } … }` blocks. The
    /// checker hoists each method into the regular
    /// function table with name mangled as
    /// `<TypeName>_<methodName>`. T1.2 phase 2a.
    pub methods_blocks: Vec<MethodsBlock>,
    /// `module name { items… }` blocks (closure #242). Each
    /// module groups a set of items behind a namespace so
    /// `module::item` references work from outside the
    /// module. Items inside default to private; `pub` exports
    /// them. The checker walks modules at the start of
    /// type-checking, mangles every item name to
    /// `<module>::<item>`, registers it in the global
    /// signature/type maps, and records visibility for the
    /// resolution layer. Top-level items (not inside any
    /// `module`) stay globally visible — no back-compat break.
    pub modules: Vec<ModuleDecl>,
    /// `use foo::bar;` import declarations (closure #245).
    /// Each entry introduces `bar` as an alias for the
    /// mangled `foo__bar` in the surrounding file. The
    /// checker resolves these during the module-flattening
    /// pre-pass: build a `bar → foo__bar` map, then walk
    /// top-level item bodies rewriting bare references.
    pub use_paths: Vec<UsePath>,
}

/// `module foo { fn bar() -> i64 { … } pub struct Point {…} }`
/// — closure #242 namespace declaration. v1 carries only
/// top-level items (no nested modules); the parser will reject
/// `module a { module b { … } }`. The checker flattens this
/// into the existing `Program::functions` / `structs` / etc.
/// with mangled names.
#[derive(Clone, Debug, PartialEq)]
pub struct ModuleDecl {
    pub name: String,
    pub name_span: Span,
    pub functions: Vec<Function>,
    pub structs: Vec<StructDecl>,
    pub enums: Vec<EnumDecl>,
    pub interfaces: Vec<InterfaceDecl>,
    pub impls: Vec<ImplDecl>,
    pub consts: Vec<ConstDecl>,
    pub type_aliases: Vec<TypeAlias>,
    pub methods_blocks: Vec<MethodsBlock>,
    /// Closure #248: nested module declarations. A module
    /// can contain other modules, building up the
    /// `outer::inner::leaf` path hierarchy. The checker
    /// recursively flattens — items in `outer::inner` get
    /// mangled to `outer__inner__name`.
    pub modules: Vec<ModuleDecl>,
    /// Parallel to each field above; `true` if the item is
    /// `pub` (exported). Top-level items have no entry here —
    /// they're globally visible by default. Index ordering
    /// matches the corresponding Vec.
    pub visibility: ModuleVisibility,
    pub span: Span,
}

/// Per-item-list visibility bitmaps for a `ModuleDecl`.
/// Indices align with the same-named Vec in `ModuleDecl`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModuleVisibility {
    pub functions_pub: Vec<bool>,
    pub structs_pub: Vec<bool>,
    pub enums_pub: Vec<bool>,
    pub interfaces_pub: Vec<bool>,
    pub impls_pub: Vec<bool>,
    pub consts_pub: Vec<bool>,
    pub type_aliases_pub: Vec<bool>,
    pub methods_blocks_pub: Vec<bool>,
    /// Parallel visibility for nested modules.
    pub modules_pub: Vec<bool>,
}

/// `methods on Point { fn dist(self: Point) -> i64 { … } }`
/// — group of methods attached to a concrete type. T1.2.
#[derive(Clone, Debug, PartialEq)]
pub struct MethodsBlock {
    pub for_type: Type,
    pub for_type_span: Span,
    pub methods: Vec<Function>,
    pub span: Span,
}

/// `type Coord = (i64, i64);` — a name for an existing
/// type. v1 rejects recursive aliases and forward
/// references. T4.15.
#[derive(Clone, Debug, PartialEq)]
pub struct TypeAlias {
    pub name: String,
    pub name_span: Span,
    pub target: Type,
    pub span: Span,
}

/// `const PI: f64 = 3.14159;` — compile-time constant value
/// scoped to the whole program. T4.15.
#[derive(Clone, Debug, PartialEq)]
pub struct ConstDecl {
    pub name: String,
    pub name_span: Span,
    pub ty: Type,
    pub value: Expr,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StructDecl {
    pub name: String,
    pub name_span: Span,
    pub fields: Vec<StructField>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StructField {
    pub name: String,
    pub ty: Type,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EnumDecl {
    pub name: String,
    pub name_span: Span,
    pub variants: Vec<EnumVariant>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EnumVariant {
    pub name: String,
    pub name_span: Span,
    /// Payload type list. Empty for payload-less variants
    /// (`Red`, `Green`, `None`). Single-element for tuple-1
    /// variants (`Some(T)`, `Ok(T)`). Multi for tuple-N
    /// (`Err(code: i64, msg: String)` → represented as
    /// positional types, names land in phase 2b). T1.3 phase 2a.
    pub payload: Vec<Type>,
}

/// `interface Cmp { fn cmp(self, other: ref Self) returns i64; }`
/// Declares abstract methods that types can opt into via
/// `implement Cmp for Point { … }`. v1: no inheritance, no
/// default methods, no associated types. T1.5.
#[derive(Clone, Debug, PartialEq)]
pub struct InterfaceDecl {
    pub name: String,
    pub name_span: Span,
    pub methods: Vec<InterfaceMethod>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InterfaceMethod {
    pub name: String,
    pub name_span: Span,
    pub params: Vec<Param>,
    pub return_type: Type,
    pub span: Span,
}

/// `implement Cmp for Point { fn cmp(self, other: ref Self) returns i64 { … } }`
/// Binds an interface's methods to a concrete type. T1.5.
#[derive(Clone, Debug, PartialEq)]
pub struct ImplDecl {
    pub interface_name: String,
    pub for_type: Type,
    pub methods: Vec<Function>,
    pub span: Span,
    /// Closure #246: the module name this impl was declared
    /// inside, or `None` for a top-level impl. Populated by
    /// `flatten_modules_in_program`. Consulted by the orphan-
    /// rule check in `hoist_impls_into_functions`: an impl
    /// must be in the same module as either its interface or
    /// its for-type (or all three must be top-level).
    pub home_module: Option<String>,
}

/// `where T is Cmp` — interface bound on a generic type
/// parameter. v1 allows one bound per parameter; phase 2
/// may lift to `T is Cmp + Hash`. T1.5.
#[derive(Clone, Debug, PartialEq)]
pub struct WhereClause {
    pub type_param: String,
    pub interface_name: String,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Use {
    pub path: String,
    pub span: Span,
}

/// `use foo::bar;` and friends. Closure #245 introduced the
/// single-item form; #247 added the multi-item brace list
/// (`use foo::{a, b};`); #253 added the glob (`use foo::*;`)
/// — `item == "*"` is the sentinel the checker expands to all
/// direct public children of `foo` at flatten time. #254 added
/// the optional `as <local>` rename suffix (single-item and
/// per-entry brace-list both honor it) plus a collision
/// diagnostic when two imports bind the same local name. The
/// checker rewrites bare references in top-level fn bodies as
/// the mangled name `foo__bar` (or the matching private form
/// `foo__priv__bar` for intra-module references — but
/// path-imports from OUTSIDE the module can only target public
/// items by construction, since private items can't form a
/// valid source-level path).
#[derive(Clone, Debug, PartialEq)]
pub struct UsePath {
    pub module: String,
    pub item: String,
    /// Optional local rename — `use foo::bar as baz;` binds
    /// `baz` to `foo__bar` instead of `bar`. None for plain
    /// imports. Closure #254 — lets users resolve name
    /// collisions between two modules exporting the same
    /// leaf, and gives code a clearer local name without
    /// re-using the (possibly conflicting) original.
    pub alias: Option<String>,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Intent {
    pub text: String,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Function {
    pub name: String,
    /// Type-parameter names declared after the fn name —
    /// `fn first<T>(...)` puts `["T"]` here. Empty for
    /// non-generic functions. v1 has no bounds; T1.5 adds
    /// `where T is Cmp` constraints. T1.4.
    pub type_params: Vec<String>,
    /// `where T is Cmp` clauses on a generic function. Empty
    /// for non-bounded generics or non-generic fns. T1.5.
    pub where_clauses: Vec<WhereClause>,
    pub params: Vec<Param>,
    pub return_type: Type,
    pub requires: Vec<Expr>,
    pub ensures: Vec<Expr>,
    pub body: Vec<Stmt>,
    pub span: Span,
    /// Set by the parser when the function declaration was written
    /// `pure fn name(...)`. The checker enforces a side-effect-free
    /// body: no `print`, no IndexAssign, no consuming Vec mutators,
    /// and no call to a non-pure function. Calls from a `parallel
    /// for` body must target a pure function — the absence of
    /// shared mutable state then proves data-race freedom.
    pub is_pure: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Param {
    pub name: String,
    pub ty: Type,
    /// Source-byte span of the parameter's name identifier
    /// only (no surrounding type annotation or punctuation).
    /// LSP features (semantic tokens, goto-def) use this to
    /// highlight / navigate to the parameter precisely.
    pub name_span: Span,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Type {
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    F32,
    F64,
    Bool,
    /// Immutable, NUL-terminated string. Borrowed (Copy) pointer
    /// to either a static string literal or an `OwnedStr`'s
    /// underlying buffer. Always safe to pass and re-use; can be
    /// compared with `==`/`<`/etc. (via `strcmp`) and queried with
    /// `len(s)`.
    Str,
    /// Owned, heap-allocated, NUL-terminated string. Produced by
    /// the `+` concat operator on `Str` operands; affine (single-
    /// use) and freed at scope exit unless moved/consumed.
    /// Implicit borrow to `Str` is not yet implemented — pass to
    /// `Str` parameters via the same value when both forms are
    /// acceptable.
    OwnedStr,
    Array {
        element: Box<Type>,
        length: u64,
    },
    Vec(Box<Type>),
    /// Tuple `(T1, T2, …, Tn)` — fixed-size heterogeneous product.
    /// v1 caps `n` at 4 elements, all elements must be `Copy`, and
    /// the only way to extract is destructuring let
    /// (`let (a, b) = pair;`). Refines T1.1. Non-Copy elements and
    /// `.0` field access are tracked as follow-ups.
    Tuple(Vec<Type>),
    /// User-declared record type. Nominal — equality is by
    /// name, not shape. The struct's fields live in the
    /// program-level `StructDecl` registry; the checker
    /// looks them up by name. v1 caps at 1..=64 fields, all
    /// fields must be `Copy`, and there's no field-update
    /// syntax (`p.x = …` is rejected). Refines T1.2.
    Struct(String),
    /// User-declared enum (tagged union). v1 ships payload-
    /// less variants only; payload variants land in T1.3
    /// phase 2. The enum's variants live in the program-level
    /// `EnumDecl` registry keyed by name. Refines T1.3.
    Enum(String),
    /// Type parameter — placeholder filled in at
    /// monomorphization. Only ever appears inside a generic
    /// function's signature / body during checking; by the
    /// time the typed IR reaches the backends every
    /// `Type::Param` has been substituted with a concrete
    /// type. Refines T1.4.
    Param(String),
    Ref(Box<Type>),
    RefMut(Box<Type>),
    /// Handle to a spawned `task <name> { … }`. Affine: each
    /// handle must be consumed by exactly one `join <name>;` in
    /// the same block. v1 has no payload — `Task` is structural
    /// only — so the type is non-parametric.
    Task,
    /// `Atomic<T>` — opt-in atomic cell. The four builtin
    /// operations (`atomic_new`, `atomic_load`, `atomic_store`,
    /// `atomic_fetch_add`) all promise sequentially-consistent
    /// ordering across threads. v1 supports `Atomic<i64>` only;
    /// other widths follow the same template. The cell is
    /// affine (different cells have different identities;
    /// copying would silently de-share state), so ops take
    /// `&Atomic<T>`.
    Atomic(Box<Type>),
    /// `Channel<T>` / `Channel<T, N>` — affine handle to an
    /// N-slot bounded ring buffer (Vyukov MPSC). `Channel<T>`
    /// without an explicit capacity defaults to N = 16. N
    /// must be a power of two ≥ 1 (the runtime uses
    /// `t & (N-1)` masking). T ranges over the integer
    /// widths i8..i64 / u8..u64 — checked at construction.
    /// Operations: `channel_new() -> Channel<T, N>` (owned),
    /// `channel_send` (publishes), `channel_recv` (consumes).
    Channel(Box<Type>, u64),
    /// `Mutex<T>` — affine handle to a value protected by a
    /// spin-lock. Direct ops are gated behind a `Guard<T>`
    /// obtained from `mutex_lock(&m) -> Guard<T>`; the guard's
    /// scope-exit drop releases the lock automatically (the
    /// RAII pattern). v1: `Mutex<i64>` only.
    Mutex(Box<Type>),
    /// `Guard<T>` — affine handle returned by `mutex_lock`.
    /// While alive, the holding thread has exclusive access to
    /// the protected `T` via `guard_get(&g)` / `guard_set(&g, v)`.
    /// Dropped at scope exit (the checker emits a `Drop` TypedStmt
    /// which the backends lower to `mutex_unlock`). v1: i64
    /// payload.
    Guard(Box<Type>),
    /// `fn(T1, T2, ...) -> R` — first-class function pointer
    /// over user-defined `fn` declarations. Copyable (a fn
    /// pointer is the same machine word every time you take
    /// it). Indirect calls through a fn-ptr bypass the
    /// name-based call-graph passes (locks_params propagation,
    /// purity checks), so the checker conservatively rejects
    /// indirect calls inside contexts that need those guarantees
    /// (lock holders + pure bodies — see TODO #A3 follow-up).
    FnPtr(Vec<Type>, Box<Type>),
    /// `dyn IfaceName` — fat pointer to a type implementing
    /// the given interface. Layout is `{ &vtable, &data }`;
    /// method calls dispatch through the vtable. Copyable
    /// (two pointer-sized fields). Composition-only — no
    /// inheritance, no parent-class walks. Epic A Phase 1.
    Object(String),
}

impl Type {
    pub fn is_integer(&self) -> bool {
        matches!(
            self,
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

    pub fn is_signed_integer(&self) -> bool {
        matches!(self, Type::I8 | Type::I16 | Type::I32 | Type::I64)
    }

    pub fn is_unsigned_integer(&self) -> bool {
        matches!(self, Type::U8 | Type::U16 | Type::U32 | Type::U64)
    }

    pub fn is_float(&self) -> bool {
        matches!(self, Type::F32 | Type::F64)
    }

    pub fn is_numeric(&self) -> bool {
        self.is_integer() || self.is_float()
    }

    pub fn is_array(&self) -> bool {
        matches!(self, Type::Array { .. })
    }

    pub fn is_vec(&self) -> bool {
        matches!(self, Type::Vec(_))
    }

    pub fn is_ref(&self) -> bool {
        matches!(self, Type::Ref(_))
    }

    pub fn is_ref_mut(&self) -> bool {
        matches!(self, Type::RefMut(_))
    }

    pub fn is_any_ref(&self) -> bool {
        self.is_ref() || self.is_ref_mut()
    }

    /// Strip outer Ref(_) / RefMut(_) wrappers and return the referent.
    pub fn deref(&self) -> &Type {
        match self {
            Type::Ref(inner) | Type::RefMut(inner) => inner.deref(),
            other => other,
        }
    }

    pub fn is_copy(&self) -> bool {
        // References are Copy (cheap pointer copy). Owned aggregates
        // and OwnedStr (heap-allocated, must be freed exactly once)
        // are not Copy. `Task` is affine: each handle is consumed
        // by exactly one `join`, so it's not Copy either. `Atomic<T>`
        // owns a unique cell identity — copying would silently
        // de-share state across threads, so it's affine too.
        match self {
            Type::Array { .. }
            | Type::Vec(_)
            | Type::OwnedStr
            | Type::Task
            | Type::Atomic(_)
            | Type::Channel(_, _)
            | Type::Mutex(_)
            | Type::Guard(_) => false,
            Type::Ref(_) | Type::RefMut(_) => true,
            // Structs with at least one affine field (OwnedStr in v1)
            // are themselves affine — copying would alias the heap
            // buffer and double-free at scope exit. T1.2 phase 2b.
            Type::Struct(name) => !struct_has_non_copy_field(name),
            // Enums with at least one heap-shaped payload
            // (OwnedStr in v1) are also affine; the Drop
            // emission switches on the tag to free.
            Type::Enum(name) => !enum_has_non_copy_payload(name),
            _ => true,
        }
    }

    pub fn bits(&self) -> Option<u16> {
        match self {
            Type::I8 | Type::U8 => Some(8),
            Type::I16 | Type::U16 => Some(16),
            Type::I32 | Type::U32 => Some(32),
            Type::I64 | Type::U64 => Some(64),
            Type::F32 | Type::F64 | Type::Bool | Type::Str | Type::OwnedStr | Type::Array { .. } | Type::Vec(_) | Type::Ref(_) | Type::RefMut(_) | Type::Task | Type::Atomic(_) | Type::Channel(_, _) | Type::Mutex(_) | Type::Guard(_) | Type::FnPtr(_, _) | Type::Tuple(_) | Type::Struct(_) | Type::Enum(_) | Type::Param(_) | Type::Object(_) => None,
        }
    }

    pub fn min_value(&self) -> Option<i128> {
        match self {
            Type::I8 => Some(i8::MIN as i128),
            Type::I16 => Some(i16::MIN as i128),
            Type::I32 => Some(i32::MIN as i128),
            Type::I64 => Some(i64::MIN as i128),
            Type::U8 | Type::U16 | Type::U32 | Type::U64 => Some(0),
            Type::F32 | Type::F64 | Type::Bool | Type::Str | Type::OwnedStr | Type::Array { .. } | Type::Vec(_) | Type::Ref(_) | Type::RefMut(_) | Type::Task | Type::Atomic(_) | Type::Channel(_, _) | Type::Mutex(_) | Type::Guard(_) | Type::FnPtr(_, _) | Type::Tuple(_) | Type::Struct(_) | Type::Enum(_) | Type::Param(_) | Type::Object(_) => None,
        }
    }

    pub fn max_value(&self) -> Option<i128> {
        match self {
            Type::I8 => Some(i8::MAX as i128),
            Type::I16 => Some(i16::MAX as i128),
            Type::I32 => Some(i32::MAX as i128),
            Type::I64 => Some(i64::MAX as i128),
            Type::U8 => Some(u8::MAX as i128),
            Type::U16 => Some(u16::MAX as i128),
            Type::U32 => Some(u32::MAX as i128),
            Type::U64 => Some(u64::MAX as i128),
            Type::F32 | Type::F64 | Type::Bool | Type::Str | Type::OwnedStr | Type::Array { .. } | Type::Vec(_) | Type::Ref(_) | Type::RefMut(_) | Type::Task | Type::Atomic(_) | Type::Channel(_, _) | Type::Mutex(_) | Type::Guard(_) | Type::FnPtr(_, _) | Type::Tuple(_) | Type::Struct(_) | Type::Enum(_) | Type::Param(_) | Type::Object(_) => None,
        }
    }

    pub fn float_rank(&self) -> Option<u8> {
        match self {
            Type::F32 => Some(32),
            Type::F64 => Some(64),
            _ => None,
        }
    }
}

impl fmt::Display for Type {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Type::I8 => write!(formatter, "i8"),
            Type::I16 => write!(formatter, "i16"),
            Type::I32 => write!(formatter, "i32"),
            Type::I64 => write!(formatter, "i64"),
            Type::U8 => write!(formatter, "u8"),
            Type::U16 => write!(formatter, "u16"),
            Type::U32 => write!(formatter, "u32"),
            Type::U64 => write!(formatter, "u64"),
            Type::F32 => write!(formatter, "f32"),
            Type::F64 => write!(formatter, "f64"),
            Type::Bool => write!(formatter, "bool"),
            Type::Str => write!(formatter, "Str"),
            Type::OwnedStr => write!(formatter, "OwnedStr"),
            Type::Array { element, length } => write!(formatter, "[{}; {}]", element, length),
            Type::Vec(element) => write!(formatter, "Vec<{}>", element),
            Type::Ref(inner) => write!(formatter, "ref {}", inner),
            Type::RefMut(inner) => write!(formatter, "mut ref {}", inner),
            Type::Task => write!(formatter, "Task"),
            Type::Atomic(inner) => write!(formatter, "Atomic<{}>", inner),
            Type::Channel(inner, capacity) => {
                if *capacity == 16 {
                    write!(formatter, "Channel<{}>", inner)
                } else {
                    write!(formatter, "Channel<{}, {}>", inner, capacity)
                }
            }
            Type::Mutex(inner) => write!(formatter, "Mutex<{}>", inner),
            Type::Guard(inner) => write!(formatter, "Guard<{}>", inner),
            Type::FnPtr(params, ret) => {
                write!(formatter, "fn(")?;
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        write!(formatter, ", ")?;
                    }
                    write!(formatter, "{}", p)?;
                }
                write!(formatter, ") -> {}", ret)
            }
            Type::Tuple(elements) => {
                write!(formatter, "(")?;
                for (i, e) in elements.iter().enumerate() {
                    if i > 0 {
                        write!(formatter, ", ")?;
                    }
                    write!(formatter, "{}", e)?;
                }
                write!(formatter, ")")
            }
            Type::Struct(name) => write!(formatter, "{}", name),
            Type::Enum(name) => write!(formatter, "{}", name),
            Type::Param(name) => write!(formatter, "{}", name),
            Type::Object(iface) => write!(formatter, "dyn {}", iface),
        }
    }
}

/// One item in a `print` statement's comma-separated list.
#[derive(Clone, Debug, PartialEq)]
pub enum PrintItem {
    Expr(Expr),
    Str(String),
}

/// Operators allowed in a reduction clause. A superset of the
/// `BinaryOp` infix operators plus the function-style `min`/`max`
/// that aren't first-class operators in source code (no infix
/// syntax). Kept separate from `BinaryOp` so adding reduction
/// operators doesn't ripple through every numeric-binary match
/// arm in the language.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReductionOp {
    Add,
    Mul,
    And,
    Or,
    BitAnd,
    BitOr,
    BitXor,
    Min,
    Max,
}

impl ReductionOp {
    /// Source-level spelling used in `reduce <var> with <op>;`.
    /// Used by the formatter and by per-backend pragmas / atomics.
    pub fn display_symbol(self) -> &'static str {
        match self {
            ReductionOp::Add => "+",
            ReductionOp::Mul => "*",
            ReductionOp::And => "&&",
            ReductionOp::Or => "||",
            ReductionOp::BitAnd => "&",
            ReductionOp::BitOr => "|",
            ReductionOp::BitXor => "^",
            ReductionOp::Min => "min",
            ReductionOp::Max => "max",
        }
    }
}

/// A `reduce <var> with <op>;` clause attached to a `parallel
/// for`. The named outer binding must be updated only via the
/// declared op inside the body (verified by the effects checker);
/// each thread maintains a partial value and the runtime combines
/// them across threads at the end of the loop.
#[derive(Clone, Debug, PartialEq)]
pub struct Reduction {
    pub var: String,
    pub op: ReductionOp,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Stmt {
    Let {
        name: String,
        annotation: Option<Type>,
        expr: Expr,
        span: Span,
    },
    /// Destructuring `let (a, b, …) = expr;`. Sugar — the
    /// checker desugars into a sequence of `TypedStmt::Let`s:
    /// a temp binding holding the RHS plus one per-name `Let`
    /// reading `temp.<i>` via `TypedExprKind::TupleAccess`.
    /// TypedStmt has no matching variant. T1.1.
    LetTuple {
        names: Vec<String>,
        annotation: Option<Type>,
        expr: Expr,
        span: Span,
    },
    Return {
        expr: Expr,
        span: Span,
    },
    Assert {
        expr: Expr,
        /// Optional human-readable message emitted on runtime failure
        /// (e.g. `assert i < n, "index out of range";`).
        message: Option<String>,
        span: Span,
    },
    Prove {
        expr: Expr,
        span: Span,
    },
    /// `print item1, item2, …;` — each item is either an
    /// expression or a string literal. Items are printed in order,
    /// space-separated, terminated by a newline. The string-literal
    /// form supports basic labels (`print "x =", x;`) without yet
    /// introducing a full Str type into the type system.
    Print {
        items: Vec<PrintItem>,
        span: Span,
    },
    If {
        cond: Expr,
        then_body: Vec<Stmt>,
        else_body: Vec<Stmt>,
        span: Span,
    },
    While {
        cond: Expr,
        invariants: Vec<Expr>,
        body: Vec<Stmt>,
        span: Span,
    },
    Assign {
        name: String,
        expr: Expr,
        span: Span,
    },
    Break {
        span: Span,
    },
    Continue {
        span: Span,
    },
    IndexAssign {
        name: String,
        index: Expr,
        /// `xs[i].field = …;` — single-level field path on the
        /// indexed element. Empty for plain `xs[i] = v;`. v1
        /// supports single-level paths (struct field) and
        /// requires the Vec/array element to be a Copy struct.
        /// T1.2 phase 2b follow-up.
        field_path: Vec<String>,
        value: Expr,
        span: Span,
    },
    /// `place.field = value;` — assign through a place
    /// expression to one of its declared struct fields.
    /// The place is restricted in v1: it must be a simple
    /// `Var` (`p.x = …;`) or a borrow of one
    /// (`self.x = …;` when `self: mut ref Point`). The
    /// checker enforces the borrow's mutability and the
    /// field's existence/type. T1.2 phase 2a follow-up.
    FieldAssign {
        object: Expr,
        field: String,
        field_span: Span,
        value: Expr,
        span: Span,
    },
    For {
        var: String,
        start: Expr,
        end: Expr,
        invariants: Vec<Expr>,
        body: Vec<Stmt>,
        span: Span,
        /// Set by the parser when the loop was written `parallel
        /// for i in start..end { ... }`. The checker requires the
        /// body to be side-effect-free (same rules as `pure fn`),
        /// which makes every iteration provably independent.
        /// Backends today still lower this to a sequential loop;
        /// the verifier is the value-add until a backend follow-up
        /// adds actual threading.
        parallel: bool,
        /// `reduce <var> with <op>;` clauses (one or more) carved
        /// out of the strict pure-body rule: the body may update
        /// each declared variable via the named associative op
        /// (`+` today; other ops are easy follow-ons). The runtime
        /// gives each thread a private partial value and combines
        /// them after the loop. Only valid when `parallel` is true.
        reductions: Vec<Reduction>,
    },
    /// `for x in &xs { body }` (borrow) or `for x in xs { body }` (consume).
    /// In both cases `x` is the (Copy) element type. `consumes = true`
    /// transfers ownership of `xs` into the loop; for `Vec<T>` the buffer
    /// is freed at the end of the loop.
    ForIter {
        var: String,
        collection: String,
        consumes: bool,
        body: Vec<Stmt>,
        span: Span,
    },
    /// `task <name> { <body> }` — declares an affine `Task` handle
    /// and a side-effect-free body. The checker enforces the same
    /// purity rules as a `parallel for` body (no print, no
    /// IndexAssign, no impure calls; reductions don't apply). v1
    /// lowers the body inline at the spawn site; a real threading
    /// follow-up uses the verifier's race-freedom proof
    /// unchanged.
    TaskSpawn {
        name: String,
        body: Vec<Stmt>,
        span: Span,
    },
    /// `join <name>;` — consumes the `Task` handle named by
    /// `<name>`. The checker requires `<name>` to be in scope as
    /// `Task` and not yet joined.
    TaskJoin {
        name: String,
        span: Span,
    },
}

impl Stmt {
    pub fn span(&self) -> Span {
        match self {
            Stmt::Let { span, .. }
            | Stmt::LetTuple { span, .. }
            | Stmt::Return { span, .. }
            | Stmt::Assert { span, .. }
            | Stmt::Prove { span, .. }
            | Stmt::Print { span, .. }
            | Stmt::If { span, .. }
            | Stmt::While { span, .. }
            | Stmt::Assign { span, .. }
            | Stmt::Break { span }
            | Stmt::Continue { span }
            | Stmt::IndexAssign { span, .. }
            | Stmt::FieldAssign { span, .. }
            | Stmt::For { span, .. }
            | Stmt::ForIter { span, .. }
            | Stmt::TaskSpawn { span, .. }
            | Stmt::TaskJoin { span, .. } => *span,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ExprKind {
    Int(i128),
    Float(f64),
    Bool(bool),
    /// `"literal"` as an expression with `Type::Str`. Currently only
    /// usable in argument position (calls to functions taking
    /// `Str`) and as a print item; let-bound `Str` is deferred.
    Str(String),
    Var(String),
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    Binary {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Call {
        name: String,
        /// Source-byte span of the callee identifier only
        /// (not the `( … )` argument list). LSP semantic
        /// tokens override the lexer's default `variable`
        /// tint with `function` at this span.
        name_span: Span,
        args: Vec<Expr>,
    },
    /// `receiver.method(args)` — desugared by the checker
    /// to a regular `Call` whose name is the mangled
    /// `<TypeName>_<methodName>` and whose first argument
    /// is `receiver`. T1.2 phase 2a.
    MethodCall {
        receiver: Box<Expr>,
        method: String,
        method_span: Span,
        args: Vec<Expr>,
    },
    Cast {
        expr: Box<Expr>,
        ty: Type,
    },
    ArrayLit {
        elements: Vec<Expr>,
    },
    Index {
        array: Box<Expr>,
        index: Box<Expr>,
    },
    Len {
        array: Box<Expr>,
    },
    Ref {
        inner: Box<Expr>,
    },
    RefMut {
        inner: Box<Expr>,
    },
    /// Tuple constructor `(e1, e2, …, en)`. n in 2..=4 (v1).
    /// Lowers to a per-shape `intent_tuple_<tags>` struct
    /// build in both backends. T1.1.
    Tuple(Vec<Expr>),
    /// Tuple field read `t.0` / `t.1` / …. Synthesized by
    /// the parser when it desugars destructure-let
    /// `let (a, b) = expr;` into a temp + per-name reads.
    /// Not emitted directly by user source in v1. T1.1.
    TupleAccess {
        tuple: Box<Expr>,
        index: u32,
    },
    /// Struct literal `Name { f1: e1, f2: e2 }`. Type checker
    /// verifies all required fields are present and types
    /// match. T1.2.
    StructLit {
        type_name: String,
        type_name_span: Span,
        fields: Vec<(String, Expr)>,
    },
    /// Field read `obj.field`. Distinct from `TupleAccess`
    /// (which uses a numeric index) — `FieldAccess` carries
    /// a field name, looked up against the struct's
    /// declaration. T1.2.
    FieldAccess {
        object: Box<Expr>,
        field: String,
    },
    /// `match scrutinee { Color.Red then expr, … }` expression.
    /// Arms are exhaustive (every variant of the scrutinee's
    /// enum type must be matched). All arm RHSs must have
    /// the same type, which becomes the match expression's
    /// type. T1.3.
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<MatchArm>,
    },
    /// `if cond { expr } else { expr }` as an expression.
    /// Both branches must be a single expression in braces;
    /// statement-bearing branches stay as `Stmt::If`. The
    /// branch types must unify. T4 (if-as-expression).
    IfExpr {
        cond: Box<Expr>,
        then_value: Box<Expr>,
        else_value: Box<Expr>,
    },
    /// `{ stmt; stmt; tail-expr }` — block expression.
    /// Statements execute in order in a fresh nested scope;
    /// the tail expression's value becomes the block's value
    /// and type. Inner-scope `let` shadows don't leak (same
    /// rules as `if`/`while`/`for` bodies). Enables
    /// non-trivial `let` initializers (`let r = { let a = …;
    /// let b = …; a + b };`) and richer match-arm bodies.
    Block {
        stmts: Vec<Stmt>,
        tail: Box<Expr>,
    },
    /// `try EXPR` — error-propagation sugar. EXPR must
    /// evaluate to a payloaded enum where exactly one
    /// variant carries a payload and exactly one is
    /// payload-less. If EXPR is the payloaded variant, the
    /// inner value becomes the expression's result.
    /// Otherwise the enclosing function early-returns the
    /// payload-less variant. Requires the enclosing fn's
    /// return type to match EXPR's enum type. T2.6.
    Try {
        inner: Box<Expr>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub pattern_span: Span,
    pub body: Expr,
}

/// Match-arm pattern. T1.3 phase 1 ships payload-less variant
/// patterns, integer literal patterns, and the `_` catch-all.
/// T1.3 phase 2b adds `VariantWithBinding` for payloaded
/// destructures (`Some(x) then …`); the parser accepts the
/// syntax and the checker accepts the shape, but tagged-union
/// codegen still goes through the WIP gate until backend
/// support lands.
#[derive(Clone, Debug, PartialEq)]
pub enum Pattern {
    /// `EnumName.VariantName then …` — explicit enum variant.
    Variant { enum_name: String, variant: String },
    /// `EnumName.VariantName(binding) then …` — payloaded
    /// variant destructure. The single-binding form covers
    /// `Option<T>` / `Result<T, _>` / `Result<_, E>` patterns;
    /// multi-binding (tuple-style) variants are tracked
    /// separately. T1.3 phase 2b.
    VariantWithBinding {
        enum_name: String,
        variant: String,
        binding: String,
    },
    /// `42 then …` / `-1 then …` — integer literal
    /// pattern. Scrutinee must be an integer type; the
    /// match has no enum-style exhaustiveness check (a
    /// wildcard arm is required to cover the open set of
    /// integer values).
    Int(i128),
    /// `true then …` / `false then …` — bool literal
    /// pattern. Scrutinee must be `bool`. Exhaustiveness
    /// requires both arms OR a wildcard. T1.3 follow-up.
    Bool(bool),
    /// `"foo" then …` — string literal pattern. Scrutinee
    /// must be `Str` or `OwnedStr`; dispatch is via
    /// `strcmp`. A wildcard arm is required since the
    /// string space is open. T1.3 follow-up.
    Str(String),
    /// `_ then …` — catch-all that covers every remaining
    /// variant; must appear last in the arm list.
    Wildcard,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    Not,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Shl,
    Shr,
    BitAnd,
    BitOr,
    BitXor,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

impl BinaryOp {
    /// Source-level spelling of a binary operator. Used by the
    /// pretty-printer and by the C backend; both happen to use the
    /// same set of symbols (Rust/C/most curly-brace languages
    /// agree on `+`, `==`, `<<`, etc.). Backend-specific lowering
    /// (e.g. LLVM `icmp eq` for `==`) is the backend's
    /// responsibility, not this enum's.
    pub fn display_symbol(self) -> &'static str {
        match self {
            BinaryOp::Add => "+",
            BinaryOp::Sub => "-",
            BinaryOp::Mul => "*",
            BinaryOp::Div => "/",
            BinaryOp::Rem => "%",
            BinaryOp::Shl => "<<",
            BinaryOp::Shr => ">>",
            BinaryOp::BitAnd => "&",
            BinaryOp::BitOr => "|",
            BinaryOp::BitXor => "^",
            BinaryOp::Eq => "==",
            BinaryOp::Ne => "!=",
            BinaryOp::Lt => "<",
            BinaryOp::Le => "<=",
            BinaryOp::Gt => ">",
            BinaryOp::Ge => ">=",
            BinaryOp::And => "&&",
            BinaryOp::Or => "||",
        }
    }
}
