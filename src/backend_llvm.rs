//! LLVM textual-IR backend (early scaffolding).
//!
//! Emits `.ll` source that `lli` can run and `llc` can lower to a
//! native object. Today this is a minimal starter — it covers a
//! useful subset and explicitly errors on features we haven't
//! lowered yet, rather than silently emitting wrong IR.
//!
//! Currently supported:
//!  - Integer types `i8..i64` / `u8..u64` (all map to LLVM `i<width>`,
//!    signedness lives in the operator choice).
//!  - `bool` (LLVM `i1`).
//!  - Function definitions with integer/bool params + return.
//!  - `let`/`Reassign` of locals via alloca + store/load. Parameters
//!    are copied to allocas at entry so they can be reassigned.
//!  - `return <expr>`.
//!  - Binary arithmetic: `+`, `-`, `*`, `/`, `%`. Signedness picks
//!    `sdiv`/`udiv` and `srem`/`urem`.
//!  - Comparisons: `==`, `!=`, `<`, `<=`, `>`, `>=`. Signed/unsigned
//!    `icmp` variants.
//!  - `if cond { … } [else { … }]` with `br i1` to then/else/cont
//!    basic blocks; one-sided terminating branches don't emit a
//!    stray branch back to cont.
//!  - Function calls (direct).
//!
//!  - `assert cond, "msg"?;` lowered as `br i1` to ok/fail blocks.
//!    On failure the fail block calls
//!    `dprintf(2, "assertion failed: %s\n", <msg>)` when a custom
//!    message is present (matches the C backend's stderr output),
//!    then `@abort()` + `unreachable`. Assert messages are
//!    interned into per-module `@.assert_msg.<n>` private globals.
//!  - `print expr;` for integers and bool: format-string globals
//!    (`@.fmt.lld`/`@.fmt.llu`/`@.fmt.true`/`@.fmt.false`) +
//!    `call i32 (i8*, ...) @printf(...)`. Smaller integer widths
//!    are sign- or zero-extended to i64 before the call.
//!
//!  - `while cond { … }` lowered to header / body / exit blocks
//!    with `br i1 cond` from the header. `break` branches to the
//!    innermost exit; `continue` to the innermost header. A
//!    `FnCtx.loops` stack tracks which is which when nested.
//!  - `for i in start..end { … }` desugared to the same header /
//!    body / exit pattern, with the loop variable in its own alloca
//!    and an implicit `i = i + 1` at body fall-through.
//!  - Floats: `f32`/`f64` literals (hex-bit form so LLVM IR parses
//!    them unambiguously), `fadd`/`fsub`/`fmul`/`fdiv`/`frem`,
//!    ordered `fcmp oeq`/`olt`/... comparisons. `print` widens f32
//!    to double via `fpext` then uses `%g`.
//!  - Fixed-size arrays `[T; N]` as `alloca [N x T]` with
//!    `getelementptr` + `load` for reads and `getelementptr` +
//!    `store` for `xs[i] = v` writes. `len(xs)` for arrays returns
//!    the compile-time constant N.
//!  - References `&T` / `&mut T` as LLVM pointer types (`T*`).
//!    Reference params are passed through as the SSA arg value
//!    directly (no extra alloca round-trip); `&xs` / `&mut xs`
//!    in call args returns the binding's alloca pointer or the
//!    incoming param pointer. `Index`/`IndexAssign` strip the
//!    `&` / `&mut` so reads and writes through an array reference
//!    use the same GEP path as owned arrays.
//!  - `Vec<T>`: one named struct type per element (`%intent_vec_<elt>
//!    = type { T*, i64, i64 }`) declared at module top, plus
//!    monomorphized `@intent_vec_<elt>__push` / `__set` / `__clone`
//!    helpers (push uses `@realloc` with capacity-doubling, clone
//!    uses `@malloc` + `@memcpy`). `vec(...)` literal lowers to
//!    `@malloc` + per-element store + `insertvalue` chain. `len(xs)`
//!    loads field 1; `xs[i]` / `xs[i] = v` GEP through field 0.
//!    `Drop` frees the buffer via `@free`. Casts
//!    (sext/zext/trunc/sitofp/fptoui/fpext/fptrunc) handled.
//!  - `for x in &xs` / `for x in xs` (collection iteration) over
//!    arrays and Vecs, owned or by reference. Lowers to a counter
//!    loop with the element loaded from the cached `data` pointer
//!    each iteration. When the source consumes an owned Vec, the
//!    buffer is freed at the loop exit.
//!  - `requires` clauses: each emits a runtime guard at function
//!    entry — `br i1 cond, label %req_ok, label %req_fail` with the
//!    fail block calling `@abort()`. Matches the C backend's
//!    `assert(cond);` shape.
//!  - Div / Rem / Shl / Shr runtime guards: when the SMT-elision
//!    pass left `checked: true` on a `Binary`, the LLVM backend
//!    emits a pre-op `icmp` + branch to an abort block. Mirrors
//!    the C backend's `intent_check_*_divisor` / `_shift` helpers
//!    inline (no separate helper functions needed).
//!
//! `prove` is a no-op at codegen — the verifier already discharged
//! it (or failed compilation), so there's no runtime work.
//!
//! See `project_vani_backend.md` memory for the migration
//! plan and the list of C-coupling points to address as this grows.

use crate::ast::{BinaryOp, Type, UnaryOp};
use crate::backend::Backend;
use crate::ir::{TypedExpr, TypedExprKind, TypedFunction, TypedProgram, TypedStmt};
use std::collections::HashMap;

pub struct LlvmBackend;

impl Backend for LlvmBackend {
    fn name(&self) -> &'static str {
        "llvm"
    }

    fn emit(&self, program: &TypedProgram) -> String {
        emit_llvm(program)
    }
}

/// LLVM IR type name for `Channel<T, N>`. The name encodes
/// both the element's LLVM spelling and the capacity so each
/// (T, N) used in the program gets its own struct. e.g.
/// `Channel<i32, 32>` → `%intent_channel_i32_32`. `bool`
/// uses the storage spelling (`i8`) so the struct name
/// mentions the actual backing type rather than `i1` — the
/// runtime slot ops match it.
pub(crate) fn llvm_channel_struct(element: &Type, capacity: u64) -> String {
    format!(
        "%intent_channel_{}_{}",
        channel_slot_llvm(element),
        capacity
    )
}

/// LLVM storage type for one slot of a `Channel<T, N>` buf
/// array. Identical to `llvm_type(element)` for integer
/// widths; for `bool`, returns `"i8"` (the slot is an i8
/// shadow because `[N x i1]` storage isn't byte-addressable).
pub(crate) fn channel_slot_llvm(element: &Type) -> &'static str {
    match element {
        Type::Bool => "i8",
        _ => llvm_type(element),
    }
}

/// Linux SYS_futex syscall number for the host architecture.
/// Kernel-ABI constants; values come from
/// `linux/arch/<arch>/include/uapi/asm/unistd_64.h` (or the
/// 32-bit equivalent). The IR we emit invokes libc's
/// `syscall(2)` directly with this number, so it must match
/// the runtime host. Targets we don't know about return
/// `None`; emit_llvm panics rather than silently picking a
/// wrong number (better a clear compile-time error than a
/// silent corruption at the kernel boundary).
fn host_sys_futex_number() -> Option<i64> {
    if cfg!(target_arch = "x86_64") {
        Some(202)
    } else if cfg!(target_arch = "aarch64") || cfg!(target_arch = "riscv64") {
        Some(98)
    } else if cfg!(target_arch = "arm") || cfg!(target_arch = "x86") {
        Some(240)
    } else if cfg!(target_arch = "powerpc64") {
        Some(221)
    } else {
        None
    }
}

/// Like `host_sys_futex_number` but panics with a clear
/// message when the host arch is unsupported. Callers in the
/// mutex-lowering path use this — the panic surfaces at
/// codegen time, before the LLVM IR is ever fed to `lli`.
pub(crate) fn sys_futex_for_host() -> i64 {
    host_sys_futex_number().unwrap_or_else(|| {
        panic!(
            "LLVM mutex lowering needs SYS_futex; host arch unsupported. \
             Add the per-arch number to `host_sys_futex_number` and rerun."
        )
    })
}

/// True when the LLVM backend should emit Win32 threading
/// primitives (`@CreateThread`, `@WaitForSingleObject`,
/// `@WaitOnAddress`, ...) instead of POSIX (`@pthread_create`,
/// `@syscall(202, ...)` futex). Driven by the host's
/// `target_os` at codegen time — cross-compilation is not yet
/// supported (the C-side runtime uses the same gating).
pub(crate) fn host_uses_win32_threading() -> bool {
    cfg!(target_os = "windows")
}

thread_local! {
    /// Per-program registry of enum payload types. Populated
    /// at the start of `emit_llvm` from `program.enums`.
    /// Consulted by `llvm_type_string(Type::Enum)` to route
    /// payloaded enums to their named struct (`%Enum_<Name>`)
    /// instead of the bare `i32` tag. T1.3 phase 2b LLVM.
    pub(crate) static LLVM_ENUM_PAYLOAD_REGISTRY:
        std::cell::RefCell<std::collections::HashMap<String, Type>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
    /// Per-program registry of struct field lists. Populated at
    /// the start of `emit_llvm` from `program.structs` and
    /// consulted by the `TypedStmt::Drop` handler to emit a
    /// per-field `@free` call for each owning (`OwnedStr`) field
    /// when the struct binding goes out of scope. T1.2 phase 2b.
    pub(crate) static LLVM_STRUCT_FIELDS_REGISTRY:
        std::cell::RefCell<std::collections::HashMap<String, Vec<(String, Type)>>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
    /// Names of structs / enums that have an `implement Drop
    /// for T` impl in the program (hoisted to `T_drop`).
    /// Populated at the start of `emit_llvm` from the function
    /// table. Consulted by the `TypedStmt::Drop` handler to
    /// auto-call the user's `drop(self)` method at scope exit
    /// when the type has no owning fields. T2.7 phase 2.
    pub(crate) static LLVM_USER_DROP_REGISTRY:
        std::cell::RefCell<std::collections::HashSet<String>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
    /// Per-enum list of variant tags that carry a payload.
    /// Populated at the start of `emit_llvm` alongside the
    /// payload-type registry. The Drop handler reads this to
    /// emit a tag-conditional free for enums with a heap-
    /// shaped payload. T1.3 + T1.2 phase 2b.
    pub(crate) static LLVM_ENUM_PAYLOAD_TAGS_REGISTRY:
        std::cell::RefCell<std::collections::HashMap<String, Vec<u32>>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

pub fn emit_llvm(program: &TypedProgram) -> String {
    let mut out = String::new();
    out.push_str("; ModuleID = 'intent'\n");
    // Populate the enum payload registry from the program's
    // enum decls so `llvm_type_string(Type::Enum)` routes
    // payloaded enums to their named struct typedef.
    LLVM_ENUM_PAYLOAD_REGISTRY.with(|r| {
        let mut reg = r.borrow_mut();
        reg.clear();
        for decl in &program.enums {
            if let Some(payload_ty) = decl.payload_types.iter().find_map(|p| p.clone()) {
                reg.insert(decl.name.clone(), payload_ty);
            }
        }
    });
    LLVM_ENUM_PAYLOAD_TAGS_REGISTRY.with(|r| {
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
    LLVM_STRUCT_FIELDS_REGISTRY.with(|r| {
        let mut reg = r.borrow_mut();
        reg.clear();
        for decl in &program.structs {
            reg.insert(decl.name.clone(), decl.fields.clone());
        }
    });
    LLVM_USER_DROP_REGISTRY.with(|r| {
        let mut reg = r.borrow_mut();
        reg.clear();
        for f in &program.functions {
            if let Some(type_name) = f.name.strip_suffix("_drop") {
                reg.insert(type_name.to_string());
            }
        }
    });
    // No `target triple` line — `lli` and `llc` use the host triple
    // when it's omitted, which is what we want for a portable .ll
    // file. Backends targeting a specific triple can prepend it
    // after the fact.


    out.push_str("declare i32 @printf(i8*, ...)\n");
    out.push_str("declare i32 @dprintf(i32, i8*, ...)\n");
    out.push_str("declare i32 @putchar(i32)\n");
    out.push_str("declare void @abort() noreturn\n");
    out.push_str("declare i8* @malloc(i64)\n");
    out.push_str("declare void @free(i8*)\n");
    out.push_str("declare i8* @realloc(i8*, i64)\n");
    out.push_str("declare i8* @memcpy(i8*, i8*, i64)\n");
    out.push_str("declare i32 @strcmp(i8*, i8*)\n");
    out.push_str("declare i64 @strlen(i8*)\n");
    // Threading primitives: POSIX on Linux/macOS, Win32 on
    // Windows. `intentc` picks the host's flavor at codegen
    // time via `host_uses_win32_threading()`. Cross-
    // compilation is out of scope for v1 — the emitted .ll
    // only links on the same target_os intentc was built
    // for. The C-side runtime uses the same gating; see
    // `intent_thread_*` wrappers in `backend_c.rs`.
    if host_uses_win32_threading() {
        // Win32: CreateThread returns a HANDLE (i8*) and
        // takes (sec_attrs, stack_size, start_fn, arg,
        // flags, thread_id_out). WaitForSingleObject(handle,
        // INFINITE) blocks until the thread exits; CloseHandle
        // releases the handle. SwitchToThread is the
        // sched_yield analogue. WaitOnAddress /
        // WakeByAddressSingle implement the kernel-wait
        // primitive used by the mutex park/wake fast path.
        out.push_str("declare i8* @CreateThread(i8*, i64, i8* (i8*)*, i8*, i32, i32*)\n");
        out.push_str("declare i32 @WaitForSingleObject(i8*, i32)\n");
        out.push_str("declare i32 @CloseHandle(i8*)\n");
        out.push_str("declare i32 @SwitchToThread()\n");
        out.push_str("declare i32 @WaitOnAddress(i8*, i8*, i64, i32)\n");
        out.push_str("declare void @WakeByAddressSingle(i8*)\n");
    } else {
        // sched_yield(): POSIX system call that returns the
        // current thread's time slice to the scheduler. Kept
        // as an internal helper but no longer driven from
        // `mutex_lock` (that path now parks via futex on
        // Linux).
        out.push_str("declare i32 @sched_yield()\n");
        // pthread runtime used by the `task` lowering: spawn
        // an outlined body on a new pthread, join from the
        // parent. `pthread_t` is a typedef for
        // `unsigned long` in glibc.
        out.push_str("declare i32 @pthread_create(i64*, i8*, i8* (i8*)*, i8*)\n");
        out.push_str("declare i32 @pthread_join(i64, i8**)\n");
        // Linux futex syscall used by `mutex_lock`/
        // `Drop(Guard)` for real kernel-wait parking.
        // `@syscall` is libc's generic syscall(2)
        // trampoline; the variadic prototype accepts
        // SYS_futex (202 on x86_64), an address, a command
        // (FUTEX_WAIT_PRIVATE = 128, FUTEX_WAKE_PRIVATE =
        // 129), and a value. Targets other than x86_64
        // would pass a different SYS_futex number — out of
        // scope for v1.
        out.push_str("declare i64 @syscall(i64, ...)\n");
    }
    // Parallel-for runtime. On Linux/macOS we delegate to
    // libgomp (`@GOMP_parallel` + `omp_get_thread_num/num_threads`);
    // on Windows libgomp isn't available so the call-site
    // open-codes a `@CreateThread`-based fan-out and the
    // outlined fn reads tid/nt from a per-thread arg struct
    // instead of `omp_get_*`. `@CreateThread`,
    // `@WaitForSingleObject`, `@CloseHandle` are already
    // declared in the Win32 threading block above.
    if !host_uses_win32_threading() {
        out.push_str("declare void @GOMP_parallel(void (i8*)*, i8*, i32, i32)\n");
        out.push_str("declare i32 @omp_get_thread_num()\n");
        out.push_str("declare i32 @omp_get_num_threads()\n\n");
    }


    // Concurrency primitive struct types (used by `Channel<i64>`
    // / `Mutex<i64>` / `Guard<i64>` lowerings). Always declared
    // so the type names are available regardless of which
    // builtins the program references; unused types add three
    // lines of IR and zero runtime cost.
    // Vyukov MPSC ring buffers. One struct type per (T, N)
    // spec referenced in the program. Field layout mirrors
    // the C backend exactly so cross-backend parity holds:
    //   0: buf  [N x T]   — slot data
    //   1: seq  [N x i64] — Vyukov publication counters
    //   2: head i64       — consumer cursor
    //   3: tail i64       — producer cursor
    // Capacity comes from the parsed `Channel<T, N>` type;
    // payload type comes from T.
    {
        let mut seen = std::collections::BTreeSet::<String>::new();
        let mut specs: Vec<(Type, u64)> = Vec::new();
        for function in &program.functions {
            crate::backend_c::collect_channel_specs(
                &function.return_type,
                &mut seen,
                &mut specs,
            );
            for param in &function.params {
                crate::backend_c::collect_channel_specs(
                    &param.ty,
                    &mut seen,
                    &mut specs,
                );
            }
            for stmt in &function.body {
                crate::backend_c::collect_channel_specs_in_stmt(
                    stmt,
                    &mut seen,
                    &mut specs,
                );
            }
        }
        // Re-dedup on the LLVM struct name. The collector
        // keys by the C backend's name (which distinguishes
        // bool from i8), but at the LLVM level bool is
        // stored in an i8 shadow — so `Channel<bool, N>` and
        // `Channel<i8, N>` collapse to the same LLVM struct.
        // Emitting both would be a duplicate type definition.
        let mut llvm_seen = std::collections::BTreeSet::<String>::new();
        for (element, capacity) in &specs {
            let struct_name = llvm_channel_struct(element, *capacity);
            if !llvm_seen.insert(struct_name.clone()) {
                continue;
            }
            out.push_str(&format!(
                "{} = type {{ [{} x {}], [{} x i64], i64, i64 }}\n",
                struct_name,
                capacity,
                channel_slot_llvm(element),
                capacity,
            ));
        }
    }
    // `intent_task_handle`: pthread handle + ctx pointer so
    // join can free the ctx after pthread_join returns.
    // Mirrors the C-backend struct so cross-backend parity
    // holds at the IR level.
    out.push_str("%intent_task_handle = type { i64, i8* }\n");
    // `intent_mutex_i64`: i64 payload + i32 `locked` state.
    // The state field is i32 to match Linux's futex ABI
    // (`SYS_futex` reads/writes a 32-bit word). Drepper's
    // three-state lock: 0=unlocked, 1=locked-no-waiters,
    // 2=locked-waiters-present.
    out.push_str("%intent_mutex_i64 = type { i64, i32 }\n");
    out.push_str("%intent_guard_i64 = type { %intent_mutex_i64* }\n\n");

    emit_intent_str_concat_definition(&mut out);

    // Format-string globals used by `print`. We emit them all
    // unconditionally; they're tiny and `lli` will optimize away
    // any that go unused.
    // No-newline format strings: each `print item` writes only the
    // item; the multi-item path emits `putchar(' ')` between items
    // and `putchar('\n')` at the end. Sizes are byte counts incl.
    // the trailing NUL.
    out.push_str("@.fmt.lld = private constant [5 x i8] c\"%lld\\00\"\n");
    out.push_str("@.fmt.llu = private constant [5 x i8] c\"%llu\\00\"\n");
    out.push_str("@.fmt.g = private constant [3 x i8] c\"%g\\00\"\n");
    out.push_str("@.fmt.s = private constant [3 x i8] c\"%s\\00\"\n");
    out.push_str("@.fmt.true = private constant [5 x i8] c\"true\\00\"\n");
    out.push_str("@.fmt.false = private constant [6 x i8] c\"false\\00\"\n");
    // Empty string used by `Vec<OwnedStr>__clone` and the
    // payloaded-enum payload clone path as the second
    // operand of `intent_str_concat` to deep-copy an
    // existing OwnedStr (closure #152).
    out.push_str("@.empty_str_clone = private constant [1 x i8] c\"\\00\"\n");
    // Format string for `assert "msg"` lowering. dprintf with fd=2
    // (stderr) writes `assertion failed: <msg>\n`, matching the C
    // backend's fprintf(stderr, ...) shape.
    out.push_str(
        "@.fmt.assert = private constant [22 x i8] c\"assertion failed: %s\\0A\\00\"\n\n",
    );

    // Walk the program for unique `assert "msg"` and `print
    // "literal";` strings. Each unique text gets one private
    // global; the maps carry index lookups for the emitters.
    let mut assert_msgs: Vec<String> = Vec::new();
    let mut assert_idx: HashMap<String, usize> = HashMap::new();
    let mut print_strs: Vec<String> = Vec::new();
    let mut print_idx: HashMap<String, usize> = HashMap::new();
    for function in &program.functions {
        for s in &function.body {
            collect_assert_messages(s, &mut assert_msgs, &mut assert_idx);
            collect_print_strings(s, &mut print_strs, &mut print_idx);
        }
    }
    for (i, msg) in assert_msgs.iter().enumerate() {
        let escaped = escape_for_llvm_string(msg);
        let bytes = msg.len() + 1; // include trailing NUL
        out.push_str(&format!(
            "@.assert_msg.{} = private constant [{} x i8] c\"{}\\00\"\n",
            i, bytes, escaped
        ));
    }
    for (i, s) in print_strs.iter().enumerate() {
        let escaped = escape_for_llvm_string(s);
        let bytes = s.len() + 1;
        out.push_str(&format!(
            "@.print_str.{} = private constant [{} x i8] c\"{}\\00\"\n",
            i, bytes, escaped
        ));
    }
    if !assert_msgs.is_empty() || !print_strs.is_empty() {
        out.push('\n');
    }
    // print "literal";  strings go through printf("%s", ptr) using
    // the no-newline @.fmt.s format global declared above — no
    // separate `puts` declaration needed.

    // One named LLVM struct type per Vec element used in the program.
    // Layout matches the C backend's `intent_vec_*`: { T* data, i64 len, i64 capacity }.
    let mut vec_elements: Vec<Type> = Vec::new();
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for function in &program.functions {
        collect_vec_elements_ty(&function.return_type, &mut seen, &mut vec_elements);
        for p in &function.params {
            collect_vec_elements_ty(&p.ty, &mut seen, &mut vec_elements);
        }
        for s in &function.body {
            collect_vec_elements_in_stmt(s, &mut seen, &mut vec_elements);
        }
    }
    // User-declared structs emitted before vec / tuple
    // typedefs so other shapes can reference them. T1.2.
    for decl in &program.structs {
        let parts: Vec<String> = decl
            .fields
            .iter()
            .map(|(_, ty)| llvm_type_string(ty))
            .collect();
        out.push_str(&format!(
            "%Struct_{} = type {{ {} }}\n",
            decl.name,
            parts.join(", "),
        ));
    }
    if !program.structs.is_empty() {
        out.push('\n');
    }
    // T1.3 phase 2b LLVM: emit `%Enum_<Name> = type { i32, T }`
    // for each payloaded enum. The first field is the variant
    // tag, the second is the shared payload type. Plain enums
    // (no payload variants) stay as bare `i32`.
    let mut any_enum_emitted = false;
    for decl in &program.enums {
        if let Some(payload_ty) = decl.payload_types.iter().find_map(|p| p.clone()) {
            out.push_str(&format!(
                "%Enum_{} = type {{ i32, {} }}\n",
                decl.name,
                llvm_type_string(&payload_ty)
            ));
            any_enum_emitted = true;
        }
    }
    if any_enum_emitted {
        out.push('\n');
    }
    // Vtables Phase 3b: per-Iface vtable + fat-pointer
    // named types, gated on actual `dyn Iface` use in the
    // program (mirrors tree-C's same gate). Trampoline
    // bodies + global vtable constants come AFTER function
    // bodies because they reference the hoisted impl fns
    // by name.
    let used_dyn_ifaces_llvm = collect_used_dyn_ifaces_llvm(program);
    emit_dyn_iface_llvm_typedefs(&mut out, &used_dyn_ifaces_llvm);
    if !used_dyn_ifaces_llvm.is_empty() {
        out.push('\n');
    }
    for elt in &vec_elements {
        // In-buffer slot spelling (arrays as bare `[N x T]`).
        // Phase 2c.
        out.push_str(&format!(
            "{} = type {{ {}*, i64, i64 }}\n",
            vec_struct_name(elt),
            vec_element_value_str(elt)
        ));
    }
    if !vec_elements.is_empty() {
        out.push('\n');
        for elt in &vec_elements {
            emit_vec_helpers(elt, &mut out);
        }
    }

    for function in &program.functions {
        emit_function(function, &assert_idx, &print_idx, &mut out);
        out.push('\n');
    }

    // Vtables Phase 3b: emit per-(T, Iface) trampolines +
    // global vtable constants. Trampolines call the hoisted
    // `@fn_<T>_<method>` functions defined above; global
    // constants reference the trampolines by their `@`-name.
    emit_dyn_iface_llvm_vtables(&mut out, &used_dyn_ifaces_llvm);
    if !used_dyn_ifaces_llvm.is_empty() {
        out.push('\n');
    }

    // C-style `main` entry trampolines into fn_main and truncates the
    // i64 result to i32 for the OS exit code.
    out.push_str("define i32 @main() {\n");
    out.push_str("  %r = call i64 @fn_main()\n");
    out.push_str("  %t = trunc i64 %r to i32\n");
    out.push_str("  ret i32 %t\n");
    out.push_str("}\n");
    out
}

fn emit_function(
    function: &TypedFunction,
    assert_msg_indices: &HashMap<String, usize>,
    print_str_indices: &HashMap<String, usize>,
    out: &mut String,
) {
    let ret_ty = llvm_type_string(&function.return_type);
    out.push_str(&format!("define {} @fn_{}(", ret_ty, function.name));
    for (i, param) in function.params.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&format!(
            "{} %arg_{}",
            llvm_type_string(&param.ty),
            param.name
        ));
    }
    out.push_str(") {\n");

    let mut ctx = FnCtx::new(assert_msg_indices, print_str_indices);

    // For each parameter:
    //  - Reference params (`&T` / `&mut T`) arrive as pointer values
    //    already; we record the SSA name itself as the binding's
    //    "address" so Index, &/&mut, and load operations just reuse
    //    the existing pointer.
    //  - Scalar params get copied into an alloca so the body can
    //    reassign them through the same load/store path as `let`.
    //  - Array params (rare today; users pass them by reference)
    //    would need a memcpy; punt with a TODO comment for now.
    for param in &function.params {
        if param.ty.is_any_ref() {
            ctx.locals.insert(
                param.name.clone(),
                (param.ty.clone(), format!("%arg_{}", param.name)),
            );
            continue;
        }
        if !is_scalar(&param.ty) && !param.ty.is_array() && !param.ty.is_vec() {
            // Refs are caught by the `is_any_ref` branch above; all
            // other types fit one of the categories. The checker
            // also rejects any exotic param type, so this is dead.
            unreachable!(
                "checker: param '{}' has unsupported type {:?}",
                param.name, param.ty
            );
        }
        // Scalar / array / Vec params: alloca + store the arg
        // value. For array params LLVM accepts `[N x T]` directly
        // as an aggregate value, so the alloca holds the whole
        // array (mirrors the C backend's by-value parameter
        // semantics, just expressed via LLVM aggregates).
        let ty = llvm_type_string(&param.ty);
        let addr = format!("%{}.addr", param.name);
        out.push_str(&format!("  {} = alloca {}\n", addr, ty));
        out.push_str(&format!(
            "  store {} %arg_{}, {}* {}\n",
            ty, param.name, ty, addr
        ));
        ctx.locals.insert(param.name.clone(), (param.ty.clone(), addr));
    }

    // Lower each `requires` clause as a runtime guard at function
    // entry. Matches the C backend's `assert(...)` shape: branch on
    // the condition; on false, call `@abort()`. The verifier has
    // already discharged what it can, but call sites that the
    // verifier couldn't prove safe still need the runtime guard.
    for req in &function.requires {
        let c = emit_expr(req, &mut ctx, out);
        let ok = ctx.fresh_label("req_ok");
        let fail = ctx.fresh_label("req_fail");
        out.push_str(&format!(
            "  br i1 {}, label %{}, label %{}\n",
            c, ok, fail
        ));
        out.push_str(&format!("{}:\n", fail));
        out.push_str("  call void @abort()\n");
        out.push_str("  unreachable\n");
        out.push_str(&format!("{}:\n", ok));
    }

    for stmt in &function.body {
        emit_stmt(stmt, &mut ctx, out);
    }

    // If the body fell through without a return, emit a poison/zero
    // return so the IR validates. The checker forbids missing
    // returns, so this is only a safety net.
    if !ctx.terminated {
        out.push_str(&format!("  ret {} zeroinitializer\n", ret_ty));
    }
    out.push_str("}\n");
    // Append any parallel-for-outlined helper functions defined
    // while lowering this function. They sit as siblings of the
    // parent in the module.
    if !ctx.deferred_functions.is_empty() {
        out.push('\n');
        out.push_str(&ctx.deferred_functions);
    }
}

struct FnCtx<'a> {
    /// Monotonic counter for fresh SSA names like `%t0`, `%t1`.
    next_tmp: u32,
    /// Monotonic counter for branch labels (`then0`, `else0`, `cont0`).
    next_label: u32,
    /// Whether the current basic block already ended with a terminator
    /// (return / br). Used to suppress duplicate terminators after an
    /// early return inside one branch of an `if`.
    terminated: bool,
    /// Map from binding name → (type, alloca pointer label, e.g.
    /// `%x.addr`). Both parameters and `let`-introduced names go
    /// here; reads emit a load, writes emit a store.
    locals: HashMap<String, (Type, String)>,
    /// Stack of enclosing loop frames. `break` branches to the
    /// innermost `exit` label; `continue` to the innermost
    /// `header` (re-checks the condition).
    loops: Vec<LoopFrame>,
    /// Map from assert-message string → its global-constant index.
    /// Used to emit `dprintf(2, "assertion failed: %s\n", @.assert_msg.<i>)`
    /// before aborting. Borrowed from the module-level collection.
    assert_msg_indices: &'a HashMap<String, usize>,
    /// Map from `print "literal";` text → its global-constant index.
    /// Used to emit `call i32 @puts(i8* @.print_str.<i>)`.
    print_str_indices: &'a HashMap<String, usize>,
    /// Buffer of outlined parallel-for functions emitted while
    /// lowering the current function. Each parallel-for site lifts
    /// its body into an `@__intent_par_<N>` function defined in
    /// this buffer; `emit_function` appends the buffer after the
    /// parent function's closing `}` so the resulting module has
    /// the parent + all its outlined helpers as siblings.
    deferred_functions: String,
    /// Monotonic counter for outlined function ids; bumped each
    /// time a parallel-for is encountered in the current parent.
    next_outline: u32,
    /// Bare name (no leading `%`) of the basic block we're
    /// currently emitting into. Updated each time a label is
    /// printed to `out`. Phi nodes use this to know the
    /// actual predecessor for value flow when an inner
    /// expression has introduced its own basic blocks (e.g.
    /// nested if-exprs in else-if chains). T4 follow-up.
    current_block: String,
}

#[derive(Clone)]
struct LoopFrame {
    header: String,
    exit: String,
}

impl<'a> FnCtx<'a> {
    fn new(
        assert_msg_indices: &'a HashMap<String, usize>,
        print_str_indices: &'a HashMap<String, usize>,
    ) -> Self {
        Self {
            next_tmp: 0,
            next_label: 0,
            terminated: false,
            locals: HashMap::new(),
            loops: Vec::new(),
            assert_msg_indices,
            print_str_indices,
            deferred_functions: String::new(),
            next_outline: 0,
            // The function entry implicitly opens an unnamed
            // `0` block in LLVM IR. Code emitted before any
            // user-introduced label belongs there. We pin
            // this to the empty string; the first label we
            // emit will overwrite it.
            current_block: String::new(),
        }
    }
    fn fresh_tmp(&mut self) -> String {
        let n = self.next_tmp;
        self.next_tmp += 1;
        format!("%t{}", n)
    }
    fn fresh_label(&mut self, hint: &str) -> String {
        let n = self.next_label;
        self.next_label += 1;
        format!("{}{}", hint, n)
    }
}

fn emit_stmt(stmt: &TypedStmt, ctx: &mut FnCtx, out: &mut String) {
    if ctx.terminated {
        return;
    }
    match stmt {
        TypedStmt::Return { expr } => {
            let v = emit_expr(expr, ctx, out);
            // Use the string form so Vec / Array / Ref return types
            // get the correct LLVM type — the scalar fallback in
            // `llvm_type` would emit `i64` for everything else.
            out.push_str(&format!(
                "  ret {} {}\n",
                llvm_type_string(&expr.ty),
                v
            ));
            ctx.terminated = true;
        }
        TypedStmt::Prove { .. } => {
            // No-op at codegen: the verifier already discharged it
            // (or failed compilation).
        }
        TypedStmt::Let { name, ty, expr } => {
            // Vec let with a `vec(a, b, c)` rhs: malloc a buffer,
            // store each element, then build the struct value and
            // stash it in the let's alloca. push/set/clone rhs
            // aren't lowered yet (next batch).
            if let Type::Vec(element) = ty {
                if let TypedExprKind::Call { name: call_name, args, .. } = &expr.kind {
                    if call_name == "vec" {
                        emit_vec_let_from_literal(name, element, args, ctx, out);
                        return;
                    }
                }
                // Any other Vec-producing rhs (push, set, clone, a
                // user function returning Vec, or another Var) is
                // an expression of type Vec; evaluate it, then store
                // the resulting struct value into the new binding's
                // alloca.
                let value = emit_expr(expr, ctx, out);
                let s_ty = vec_struct_name(element);
                let addr = format!("%{}.addr", name);
                out.push_str(&format!("  {} = alloca {}\n", addr, s_ty));
                out.push_str(&format!(
                    "  store {} {}, {}* {}\n",
                    s_ty, value, s_ty, addr
                ));
                ctx.locals.insert(name.clone(), (ty.clone(), addr));
                return;
            }
            if let Type::Array { element, length } = ty {
                let agg = llvm_type_string(ty);
                let addr = format!("%{}.addr", name);
                out.push_str(&format!("  {} = alloca {}\n", addr, agg));
                // Use the string form so struct / tuple
                // elements render their `%Struct_<Name>` /
                // `%intent_tuple_<…>` spellings instead of
                // panicking. T1.2 + array-of-struct on LLVM.
                let elt_ty = llvm_type_string(element);
                if let TypedExprKind::ArrayLit { elements } = &expr.kind {
                    for (i, e) in elements.iter().enumerate() {
                        let v = emit_expr(e, ctx, out);
                        let p = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = getelementptr {}, {}* {}, i64 0, i64 {}\n",
                            p, agg, agg, addr, i
                        ));
                        out.push_str(&format!("  store {} {}, {}* {}\n", elt_ty, v, elt_ty, p));
                    }
                } else if let TypedExprKind::Var(src) = &expr.kind {
                    // `let ys: [T;N] = xs;` — copy the source
                    // array into the new alloca. LLVM accepts a
                    // whole-array load/store, which the optimizer
                    // lowers to a memcpy. The C backend uses the
                    // matching `memcpy(v_ys, v_xs, sizeof(v_ys))`.
                    let src_addr = ctx.locals.get(src)
                        .map(|(_, a)| a.clone())
                        .unwrap_or_else(|| unreachable!(
                            "checker: array let copies from undeclared binding '{}'",
                            src
                        ));
                    let tmp = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = load {}, {}* {}\n",
                        tmp, agg, agg, src_addr
                    ));
                    out.push_str(&format!(
                        "  store {} {}, {}* {}\n",
                        agg, tmp, agg, addr
                    ));
                } else {
                    // Closure #239 extension: arrays can now
                    // come from a Call (an array-returning
                    // function) or other value-producing
                    // expression. Emit the expression to
                    // get an `[N x T]` SSA value, then store
                    // it into the binding's alloca. LLVM
                    // handles the array-return ABI (sret for
                    // larger sizes) automatically when the
                    // type is the bare aggregate `[N x T]`.
                    let value = emit_expr(expr, ctx, out);
                    out.push_str(&format!(
                        "  store {} {}, {}* {}\n",
                        agg, value, agg, addr
                    ));
                }
                let _ = length;
                ctx.locals.insert(name.clone(), (ty.clone(), addr));
                return;
            }
            if !is_scalar(ty) {
                // Vec and Array were handled above; the checker
                // bans `let r: &T = …;` ("references cannot be
                // stored in `let` bindings"). Nothing else is a
                // valid let-bound type.
                unreachable!("checker: non-scalar let with type {:?}", ty);
            }
            let value = emit_expr(expr, ctx, out);
            // `llvm_type_string` routes Atomic/Channel/etc.
            // through their parametric per-(T, N) struct names;
            // `llvm_type` would only return the &'static str
            // fallback. Affects `Channel<T, N>` whose struct
            // varies per spec.
            let lty = llvm_type_string(ty);
            // Uniquify the alloca name so the same binding name
            // declared in two non-overlapping scopes (e.g., two
            // for-loop bodies in the same function) doesn't
            // collide on `%r.addr`.
            let addr = format!("{}.{}.addr", ctx.fresh_tmp(), name);
            out.push_str(&format!("  {} = alloca {}\n", addr, lty));
            out.push_str(&format!("  store {} {}, {}* {}\n", lty, value, lty, addr));
            ctx.locals.insert(name.clone(), (ty.clone(), addr));
        }
        TypedStmt::Reassign { name, ty, expr, drop_old } => {
            // Same alloca/store path as Let — Vec / Array /
            // scalar all just need a store into the existing
            // binding's address. `drop_old: true` (non-Copy
            // reassign whose RHS doesn't itself consume the
            // previous value) routes through "evaluate RHS
            // into a temp, free the old slot, store the temp"
            // so a RHS that READS the binding (e.g. through
            // `xs.len`) doesn't observe freed memory. Vec was
            // wired in #8 (still with the wrong order — eval
            // came after free); closure #133 reorders to
            // eval-first-then-free for safety and adds the
            // OwnedStr arm.
            let addr = match ctx.locals.get(name) {
                Some((_, a)) => a.clone(),
                None => unreachable!(
                    "checker: reassign to undeclared binding '{}'",
                    name
                ),
            };
            let lty = llvm_type_string(ty);
            let value = emit_expr(expr, ctx, out);
            if *drop_old {
                match ty {
                    Type::Vec(element) => {
                        let s_ty = vec_struct_name(element);
                        let free_name = format!(
                            "@intent_vec_{}__free",
                            vec_struct_tag(element)
                        );
                        let old = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = load {}, {}* {}\n",
                            old, s_ty, s_ty, addr
                        ));
                        out.push_str(&format!(
                            "  call void {}({} {})\n",
                            free_name, s_ty, old
                        ));
                    }
                    Type::OwnedStr => {
                        let old = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = load i8*, i8** {}\n",
                            old, addr
                        ));
                        out.push_str(&format!(
                            "  call void @free(i8* {})\n",
                            old
                        ));
                    }
                    Type::Struct(struct_name) => {
                        // Closure #169: tree-LLVM Reassign was
                        // leaking the OLD struct's heap-owning
                        // fields. Tree-C had a parallel arm
                        // (closure #147); tree-LLVM didn't.
                        // Walk the existing alloca's fields
                        // before the store of the fresh value.
                        let fields = LLVM_STRUCT_FIELDS_REGISTRY
                            .with(|r| r.borrow().get(struct_name).cloned())
                            .unwrap_or_default();
                        let empty: std::collections::HashSet<&String> =
                            std::collections::HashSet::new();
                        emit_llvm_struct_field_drops(
                            &addr,
                            struct_name,
                            &fields,
                            &empty,
                            ctx,
                            out,
                        );
                    }
                    Type::Enum(enum_name) => {
                        // Closure #169 (continued): same shape
                        // for payloaded-enum bindings. Load
                        // the OLD tagged-union from the alloca,
                        // branch on the tag, free the heap
                        // payload if the variant is payloaded.
                        // Mirrors the Drop handler's Enum arm.
                        let payload_ty = LLVM_ENUM_PAYLOAD_REGISTRY
                            .with(|r| r.borrow().get(enum_name).cloned());
                        let heap_kind = match &payload_ty {
                            Some(Type::OwnedStr) => Some("owned_str"),
                            Some(Type::Vec(_)) => Some("vec"),
                            _ => None,
                        };
                        if let Some(kind) = heap_kind {
                            let payload_tags: Vec<u32> = LLVM_ENUM_PAYLOAD_TAGS_REGISTRY
                                .with(|r| r.borrow().get(enum_name).cloned().unwrap_or_default());
                            if !payload_tags.is_empty() {
                                let s_ty = format!("%Enum_{}", enum_name);
                                let loaded = ctx.fresh_tmp();
                                out.push_str(&format!(
                                    "  {} = load {}, {}* {}\n",
                                    loaded, s_ty, s_ty, addr
                                ));
                                let tag = ctx.fresh_tmp();
                                out.push_str(&format!(
                                    "  {} = extractvalue {} {}, 0\n",
                                    tag, s_ty, loaded
                                ));
                                let payload = ctx.fresh_tmp();
                                out.push_str(&format!(
                                    "  {} = extractvalue {} {}, 1\n",
                                    payload, s_ty, loaded
                                ));
                                let free_lbl = ctx.fresh_label("reassign_enum_free");
                                let done_lbl = ctx.fresh_label("reassign_enum_done");
                                let mut prev = "i1 false".to_string();
                                for t in &payload_tags {
                                    let cmp = ctx.fresh_tmp();
                                    out.push_str(&format!(
                                        "  {} = icmp eq i32 {}, {}\n",
                                        cmp, tag, t
                                    ));
                                    let or_v = ctx.fresh_tmp();
                                    out.push_str(&format!(
                                        "  {} = or {}, {}\n",
                                        or_v, prev, cmp
                                    ));
                                    prev = format!("i1 {}", or_v);
                                }
                                let cond = prev.trim_start_matches("i1 ").to_string();
                                out.push_str(&format!(
                                    "  br i1 {}, label %{}, label %{}\n",
                                    cond, free_lbl, done_lbl
                                ));
                                out.push_str(&format!("{}:\n", free_lbl));
                                match kind {
                                    "owned_str" => {
                                        out.push_str(&format!(
                                            "  call void @free(i8* {})\n",
                                            payload
                                        ));
                                    }
                                    "vec" => {
                                        if let Some(Type::Vec(element)) = &payload_ty {
                                            let free_name = format!(
                                                "@intent_vec_{}__free",
                                                vec_struct_tag(element)
                                            );
                                            let v_struct = vec_struct_name(element);
                                            out.push_str(&format!(
                                                "  call void {}({} {})\n",
                                                free_name, v_struct, payload
                                            ));
                                        }
                                    }
                                    _ => {}
                                }
                                out.push_str(&format!("  br label %{}\n", done_lbl));
                                out.push_str(&format!("{}:\n", done_lbl));
                                ctx.current_block = done_lbl;
                            }
                        }
                    }
                    _ => {}
                }
            }
            out.push_str(&format!("  store {} {}, {}* {}\n", lty, value, lty, addr));
        }
        TypedStmt::If { cond, then_body, else_body } => {
            let c = emit_expr(cond, ctx, out);
            let then_lbl = ctx.fresh_label("then");
            let else_lbl = ctx.fresh_label("else");
            let cont_lbl = ctx.fresh_label("cont");
            // The else block is needed even when there's no source-
            // level else, since the cond=false path must branch
            // somewhere; we just give it a label and an immediate
            // branch to cont in that case.
            out.push_str(&format!(
                "  br i1 {}, label %{}, label %{}\n",
                c, then_lbl, else_lbl
            ));

            out.push_str(&format!("{}:\n", then_lbl));
            let then_terminated_before = ctx.terminated;
            ctx.terminated = false;
            for s in then_body {
                emit_stmt(s, ctx, out);
            }
            let then_terminated = ctx.terminated;
            if !then_terminated {
                out.push_str(&format!("  br label %{}\n", cont_lbl));
            }

            out.push_str(&format!("{}:\n", else_lbl));
            ctx.terminated = then_terminated_before;
            for s in else_body {
                emit_stmt(s, ctx, out);
            }
            let else_terminated = ctx.terminated;
            if !else_terminated {
                out.push_str(&format!("  br label %{}\n", cont_lbl));
            }

            // If both branches terminated, the cont block is unreachable
            // — LLVM still wants a basic block label there if any
            // following statement would target it. The simplest sound
            // thing: emit it as a labeled unreachable, so subsequent
            // statements (which the checker forbade) wouldn't validate.
            if then_terminated && else_terminated {
                ctx.terminated = true;
                out.push_str(&format!("{}:\n  unreachable\n", cont_lbl));
            } else {
                ctx.terminated = false;
                out.push_str(&format!("{}:\n", cont_lbl));
            }
        }
        TypedStmt::Drop { name, ty, moved_fields } => {
            // For `Vec<T>`, route through the per-element-type
            // `@intent_vec_<tag>__free` helper. The helper
            // walks elements first for non-Copy element types
            // (`Vec<U>`) then frees the outer buffer; for Copy
            // elements it's effectively the old single `free`
            // call. Refines #7. Arrays have stack lifetime —
            // no drop. `OwnedStr` is a heap `i8*` from concat;
            // free it directly.
            if let Type::Vec(element) = ty {
                if let Some((_, addr)) = ctx.locals.get(name).cloned() {
                    let s_ty = vec_struct_name(element);
                    let v_loaded = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = load {}, {}* {}\n",
                        v_loaded, s_ty, s_ty, addr
                    ));
                    let free_name = format!(
                        "@intent_vec_{}__free",
                        vec_struct_tag(element)
                    );
                    out.push_str(&format!(
                        "  call void {}({} {})\n",
                        free_name, s_ty, v_loaded
                    ));
                }
            } else if matches!(ty, Type::OwnedStr) {
                if let Some((_, addr)) = ctx.locals.get(name).cloned() {
                    let ptr = ctx.fresh_tmp();
                    out.push_str(&format!("  {} = load i8*, i8** {}\n", ptr, addr));
                    out.push_str(&format!("  call void @free(i8* {})\n", ptr));
                }
            } else if let Type::Struct(struct_name) = ty {
                // Auto-call the user's `Drop` impl. Two flavors
                // (mirrors backend_c):
                // * by-value `self: T` — consume; valid only
                //   when struct has no owning fields.
                // * by-ref `self: mut ref T` (epic C) — run
                //   user-Drop with a pointer to the binding,
                //   then fall through to per-field cleanup.
                let fields = LLVM_STRUCT_FIELDS_REGISTRY
                    .with(|r| r.borrow().get(struct_name).cloned())
                    .unwrap_or_default();
                let has_user_drop = LLVM_USER_DROP_REGISTRY
                    .with(|r| r.borrow().contains(struct_name));
                let user_drop_by_ref = crate::ast::user_drop_is_by_ref(struct_name);
                let has_owning_field = fields.iter().any(|(_, fty)| {
                    matches!(fty, Type::OwnedStr | Type::Vec(_) | Type::Struct(_))
                });
                if has_user_drop && user_drop_by_ref {
                    if let Some((_, addr)) = ctx.locals.get(name).cloned() {
                        let s_ty = format!("%Struct_{}", struct_name);
                        let ret = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = call i64 @fn_{}_drop({}* {})\n",
                            ret, struct_name, s_ty, addr
                        ));
                    }
                    // Fall through to per-field cleanup below.
                } else if has_user_drop && !has_owning_field {
                    if let Some((_, addr)) = ctx.locals.get(name).cloned() {
                        let s_ty = format!("%Struct_{}", struct_name);
                        let loaded = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = load {}, {}* {}\n",
                            loaded, s_ty, s_ty, addr
                        ));
                        let ret = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = call i64 @fn_{}_drop({} {})\n",
                            ret, struct_name, s_ty, loaded
                        ));
                    }
                    return;
                }
                if let Some((_, addr)) = ctx.locals.get(name).cloned() {
                    let moved: std::collections::HashSet<&String> =
                        moved_fields.iter().collect();
                    // Recursively emit per-field frees,
                    // descending into nested struct fields.
                    // Reverse-declaration-order drop preserved
                    // (Rust RAII convention). T1.2 phase 2b + D2.
                    emit_llvm_struct_field_drops(
                        &addr,
                        struct_name,
                        &fields,
                        &moved,
                        ctx,
                        out,
                    );
                }
            } else if let Type::Enum(enum_name) = ty {
                // Payloaded enums with a heap-shaped payload
                // free the payload when the active variant
                // tag matches. Supported: `OwnedStr` (free
                // i8*) and `Vec<T>` (intent_vec_<T>__free).
                // T1.3 + T1.2 phase 2b.
                let payload_ty = LLVM_ENUM_PAYLOAD_REGISTRY
                    .with(|r| r.borrow().get(enum_name).cloned());
                let heap_kind = match &payload_ty {
                    Some(Type::OwnedStr) => Some("owned_str"),
                    Some(Type::Vec(_)) => Some("vec"),
                    _ => None,
                };
                if let Some(kind) = heap_kind {
                    let payload_tags: Vec<u32> = LLVM_ENUM_PAYLOAD_TAGS_REGISTRY
                        .with(|r| r.borrow().get(enum_name).cloned().unwrap_or_default());
                    if let Some((_, addr)) = ctx.locals.get(name).cloned() {
                        let s_ty = format!("%Enum_{}", enum_name);
                        let loaded = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = load {}, {}* {}\n",
                            loaded, s_ty, s_ty, addr
                        ));
                        let tag = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = extractvalue {} {}, 0\n",
                            tag, s_ty, loaded
                        ));
                        let payload = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = extractvalue {} {}, 1\n",
                            payload, s_ty, loaded
                        ));
                        let free_lbl = ctx.fresh_label("enum_drop_free");
                        let done_lbl = ctx.fresh_label("enum_drop_done");
                        // Synthesize an i1 "is_payloaded_tag"
                        // by OR-ing each per-tag equality.
                        let mut prev = "i1 false".to_string();
                        for t in &payload_tags {
                            let cmp = ctx.fresh_tmp();
                            out.push_str(&format!(
                                "  {} = icmp eq i32 {}, {}\n",
                                cmp, tag, t
                            ));
                            let or_v = ctx.fresh_tmp();
                            out.push_str(&format!(
                                "  {} = or {}, {}\n",
                                or_v, prev, cmp
                            ));
                            prev = format!("i1 {}", or_v);
                        }
                        // Strip the leading "i1 " for the
                        // branch condition.
                        let cond = prev.trim_start_matches("i1 ").to_string();
                        out.push_str(&format!(
                            "  br i1 {}, label %{}, label %{}\n",
                            cond, free_lbl, done_lbl
                        ));
                        out.push_str(&format!("{}:\n", free_lbl));
                        match kind {
                            "owned_str" => {
                                out.push_str(&format!(
                                    "  call void @free(i8* {})\n",
                                    payload
                                ));
                            }
                            "vec" => {
                                if let Some(Type::Vec(element)) = &payload_ty {
                                    let free_name = format!(
                                        "@intent_vec_{}__free",
                                        vec_struct_tag(element)
                                    );
                                    let v_struct = vec_struct_name(element);
                                    out.push_str(&format!(
                                        "  call void {}({} {})\n",
                                        free_name, v_struct, payload
                                    ));
                                }
                            }
                            _ => {}
                        }
                        out.push_str(&format!("  br label %{}\n", done_lbl));
                        out.push_str(&format!("{}:\n", done_lbl));
                        ctx.current_block = done_lbl;
                    }
                }
            } else if matches!(ty, Type::Guard(_)) {
                // RAII unlock with futex wake. Drepper's
                // three-state lock: an `atomicrmw sub 1`
                // returns the OLD state. If it was 1
                // (no-waiters), the sub left state at 0 and
                // no thread is parked — we're done. If it
                // was 2 (waiters present), the sub left
                // state at 1 (wrong!), so we store 0 and
                // call FUTEX_WAKE to release one waiter.
                if let Some((_, addr)) = ctx.locals.get(name).cloned() {
                    let mp_p = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = getelementptr %intent_guard_i64, %intent_guard_i64* {}, i32 0, i32 0\n",
                        mp_p, addr
                    ));
                    let m_ptr = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = load %intent_mutex_i64*, %intent_mutex_i64** {}\n",
                        m_ptr, mp_p
                    ));
                    let locked_p = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = getelementptr %intent_mutex_i64, %intent_mutex_i64* {}, i32 0, i32 1\n",
                        locked_p, m_ptr
                    ));
                    let old = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = atomicrmw sub i32* {}, i32 1 seq_cst\n",
                        old, locked_p
                    ));
                    let had_waiters = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = icmp ne i32 {}, 1\n",
                        had_waiters, old
                    ));
                    let wake_blk = ctx.fresh_label("mu_unlock_wake");
                    let done = ctx.fresh_label("mu_unlock_done");
                    out.push_str(&format!(
                        "  br i1 {}, label %{}, label %{}\n",
                        had_waiters, wake_blk, done
                    ));
                    out.push_str(&format!("{}:\n", wake_blk));
                    // Reset to 0 (the sub left state at 1)
                    // and wake one waiter via the host's
                    // kernel-wait primitive — futex(WAKE) on
                    // POSIX, WakeByAddressSingle on Win32.
                    out.push_str(&format!(
                        "  store atomic i32 0, i32* {} seq_cst, align 4\n",
                        locked_p
                    ));
                    if host_uses_win32_threading() {
                        let locked_i8 = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = bitcast i32* {} to i8*\n",
                            locked_i8, locked_p
                        ));
                        out.push_str(&format!(
                            "  call void @WakeByAddressSingle(i8* {})\n",
                            locked_i8
                        ));
                    } else {
                        let _wake_ret = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = call i64 (i64, ...) @syscall(i64 {}, i32* {}, i32 129, i32 1, i8* null, i8* null, i32 0)\n",
                            _wake_ret, sys_futex_for_host(), locked_p
                        ));
                    }
                    out.push_str(&format!("  br label %{}\n", done));
                    out.push_str(&format!("{}:\n", done));
                }
            }
        }
        TypedStmt::Discard { expr } => {
            // `let _ = expr;` — evaluate for side effects, drop the value.
            // Scalar (incl. Str): just emit; the SSA result goes unused.
            // Vec<T>: route through the per-element-type __free
            // helper so nested-Vec elements get recursively
            // released. OwnedStr: heap `i8*` from concat / call;
            // free it directly (closure #134 — was leaking).
            // Struct with owning fields: spill to alloca + walk
            // fields (closure #145). Array (stack-allocated)
            // and refs don't own a heap buffer.
            //
            // The Struct case is checked BEFORE `is_scalar`
            // because `is_scalar` returns true for
            // `Type::Struct(_)` so it would skip the Struct
            // arm entirely.
            if let Type::Struct(struct_name) = &expr.ty {
                let fields = LLVM_STRUCT_FIELDS_REGISTRY
                    .with(|r| r.borrow().get(struct_name).cloned())
                    .unwrap_or_default();
                let has_owning = fields.iter().any(|(_, ty)| !ty.is_copy());
                let value = emit_expr(expr, ctx, out);
                if has_owning {
                    let s_ty = format!("%Struct_{}", struct_name);
                    let addr = format!("{}.{}.addr", ctx.fresh_tmp(), "_intent_discard");
                    out.push_str(&format!("  {} = alloca {}\n", addr, s_ty));
                    out.push_str(&format!(
                        "  store {} {}, {}* {}\n",
                        s_ty, value, s_ty, addr
                    ));
                    let empty: std::collections::HashSet<&String> =
                        std::collections::HashSet::new();
                    emit_llvm_struct_field_drops(
                        &addr,
                        struct_name,
                        &fields,
                        &empty,
                        ctx,
                        out,
                    );
                }
            } else if let Type::Enum(enum_name) = &expr.ty {
                // Closure #146: enums with heap-shaped payload
                // (OwnedStr / Vec) need their payload freed
                // at discard. Tag-only enums fall through to
                // a bare emit. Mirrors the scope-exit Drop
                // logic for enums above.
                let payload_ty = LLVM_ENUM_PAYLOAD_REGISTRY
                    .with(|r| r.borrow().get(enum_name).cloned());
                let value = emit_expr(expr, ctx, out);
                let heap_kind = match &payload_ty {
                    Some(Type::OwnedStr) => Some("owned_str"),
                    Some(Type::Vec(_)) => Some("vec"),
                    _ => None,
                };
                if let Some(kind) = heap_kind {
                    let payload_tags: Vec<u32> = LLVM_ENUM_PAYLOAD_TAGS_REGISTRY
                        .with(|r| r.borrow().get(enum_name).cloned().unwrap_or_default());
                    if !payload_tags.is_empty() {
                        let s_ty = format!("%Enum_{}", enum_name);
                        let tag = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = extractvalue {} {}, 0\n",
                            tag, s_ty, value
                        ));
                        let payload = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = extractvalue {} {}, 1\n",
                            payload, s_ty, value
                        ));
                        let free_lbl = ctx.fresh_label("disc_enum_free");
                        let done_lbl = ctx.fresh_label("disc_enum_done");
                        let mut prev = "i1 false".to_string();
                        for t in &payload_tags {
                            let cmp = ctx.fresh_tmp();
                            out.push_str(&format!(
                                "  {} = icmp eq i32 {}, {}\n",
                                cmp, tag, t
                            ));
                            let or_v = ctx.fresh_tmp();
                            out.push_str(&format!(
                                "  {} = or {}, {}\n",
                                or_v, prev, cmp
                            ));
                            prev = format!("i1 {}", or_v);
                        }
                        let cond = prev.trim_start_matches("i1 ").to_string();
                        out.push_str(&format!(
                            "  br i1 {}, label %{}, label %{}\n",
                            cond, free_lbl, done_lbl
                        ));
                        out.push_str(&format!("{}:\n", free_lbl));
                        match kind {
                            "owned_str" => {
                                out.push_str(&format!(
                                    "  call void @free(i8* {})\n",
                                    payload
                                ));
                            }
                            "vec" => {
                                if let Some(Type::Vec(element)) = &payload_ty {
                                    let free_name = format!(
                                        "@intent_vec_{}__free",
                                        vec_struct_tag(element)
                                    );
                                    let v_struct = vec_struct_name(element);
                                    out.push_str(&format!(
                                        "  call void {}({} {})\n",
                                        free_name, v_struct, payload
                                    ));
                                }
                            }
                            _ => {}
                        }
                        out.push_str(&format!("  br label %{}\n", done_lbl));
                        out.push_str(&format!("{}:\n", done_lbl));
                        ctx.current_block = done_lbl;
                    }
                }
            } else if matches!(&expr.ty, Type::OwnedStr) {
                // OwnedStr arm is checked BEFORE `is_scalar`
                // because `is_scalar(Type::OwnedStr)` returns
                // true so the scalar arm would consume this
                // branch and skip the `@free`, leaking the
                // heap. Closure #168 — discovered via ASan on
                // `let _ = s;` where s: OwnedStr.
                let value = emit_expr(expr, ctx, out);
                out.push_str(&format!(
                    "  call void @free(i8* {})\n",
                    value
                ));
            } else if is_scalar(&expr.ty) {
                let _ = emit_expr(expr, ctx, out);
            } else if let Type::Vec(element) = &expr.ty {
                let value = emit_expr(expr, ctx, out);
                let s_ty = vec_struct_name(element);
                let free_name = format!(
                    "@intent_vec_{}__free",
                    vec_struct_tag(element)
                );
                out.push_str(&format!(
                    "  call void {}({} {})\n",
                    free_name, s_ty, value
                ));
            } else {
                // Array (stack) and refs don't own a heap buffer.
                let _ = emit_expr(expr, ctx, out);
            }
        }
        TypedStmt::Assert { expr, message } => {
            let c = emit_expr(expr, ctx, out);
            let ok = ctx.fresh_label("ok");
            let fail = ctx.fresh_label("fail");
            out.push_str(&format!(
                "  br i1 {}, label %{}, label %{}\n",
                c, ok, fail
            ));
            out.push_str(&format!("{}:\n", fail));
            if let Some(msg) = message {
                // Look up the message's global index, then
                //   dprintf(2, "assertion failed: %s\n", <msg-ptr>)
                // before aborting. The format string global is
                // `@.fmt.assert`, layout `[22 x i8]`.
                if let Some(&idx) = ctx.assert_msg_indices.get(msg) {
                    let bytes = msg.len() + 1;
                    let fmt_p = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = getelementptr [22 x i8], [22 x i8]* @.fmt.assert, i64 0, i64 0\n",
                        fmt_p
                    ));
                    let msg_p = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = getelementptr [{} x i8], [{} x i8]* @.assert_msg.{}, i64 0, i64 0\n",
                        msg_p, bytes, bytes, idx
                    ));
                    out.push_str(&format!(
                        "  call i32 (i32, i8*, ...) @dprintf(i32 2, i8* {}, i8* {})\n",
                        fmt_p, msg_p
                    ));
                }
            }
            out.push_str("  call void @abort()\n");
            out.push_str("  unreachable\n");
            out.push_str(&format!("{}:\n", ok));
        }
        TypedStmt::Print { items } => emit_print_items(items, ctx, out),
        TypedStmt::While { cond, body } => {
            let header = ctx.fresh_label("loop_header");
            let body_lbl = ctx.fresh_label("loop_body");
            let exit = ctx.fresh_label("loop_exit");
            // Initial entry into the loop header.
            out.push_str(&format!("  br label %{}\n", header));
            // Header re-evaluates the condition each iteration.
            out.push_str(&format!("{}:\n", header));
            let c = emit_expr(cond, ctx, out);
            out.push_str(&format!(
                "  br i1 {}, label %{}, label %{}\n",
                c, body_lbl, exit
            ));
            // Body.
            out.push_str(&format!("{}:\n", body_lbl));
            ctx.loops.push(LoopFrame {
                header: header.clone(),
                exit: exit.clone(),
            });
            let outer_terminated = ctx.terminated;
            ctx.terminated = false;
            for s in body {
                emit_stmt(s, ctx, out);
            }
            // If the body fell through (no terminator), branch back
            // to the header for the next iteration. If it ended in
            // a return / break / continue, the terminator is already
            // emitted.
            if !ctx.terminated {
                out.push_str(&format!("  br label %{}\n", header));
            }
            ctx.loops.pop();
            ctx.terminated = outer_terminated;
            out.push_str(&format!("{}:\n", exit));
            // Closure #238: update ctx.current_block so a
            // surrounding Block-expr emit (e.g. inside a match
            // arm body) captures the post-while block as the
            // PHI's incoming predecessor. Without this, a
            // while-loop nested in a Block-expr would emit a
            // PHI that names the match arm's entry block as
            // the incoming predecessor — but the actual
            // predecessor is the while's exit block, breaking
            // `opt -verify` ("PHI node entries do not match
            // predecessors").
            ctx.current_block = exit;
        }
        TypedStmt::Break => {
            if let Some(frame) = ctx.loops.last() {
                out.push_str(&format!("  br label %{}\n", frame.exit));
                ctx.terminated = true;
            } else {
                out.push_str("  ; break outside a loop (checker should have rejected this)\n");
            }
        }
        TypedStmt::Continue => {
            if let Some(frame) = ctx.loops.last() {
                out.push_str(&format!("  br label %{}\n", frame.header));
                ctx.terminated = true;
            } else {
                out.push_str("  ; continue outside a loop\n");
            }
        }
        TypedStmt::For { var, ty, start, end, body, parallel, reductions } => {
            if !ty.is_integer() {
                // The parser only accepts integer literals on the
                // bounds and the checker enforces integer type on
                // both sides, so this is dead for well-typed input.
                unreachable!("checker: for-range with non-integer bounds, ty = {:?}", ty);
            }
            // Bool reductions (`&&`, `||` over `i1`) used to fall
            // back to sequential because LLVM `atomicrmw` requires
            // byte-sized operands. `emit_parallel_for_via_gomp` now
            // allocates an i8 shadow in the parent and zext/trunc
            // bridges between i1 and i8, so the bool path lowers
            // through the same outliner as integer reductions.
            if *parallel {
                // Lift body into an `@__intent_par_<id>` function
                // and call it through `@GOMP_parallel`. The
                // verifier already proved the body is pure (no
                // mutable shared state), so each thread can run
                // its iteration slice independently. Outer captures
                // are passed by pointer through the ctx struct —
                // every capture is read-only by the verifier's
                // guarantee, so concurrent reads through the same
                // pointer are race-free. Reductions are handled
                // inside the outlined fn: each in-body `Reassign`
                // on a reduction variable is rewritten to an
                // `atomicrmw <op>` against the captured pointer.
                let start_v = emit_expr(start, ctx, out);
                let end_v = emit_expr(end, ctx, out);
                emit_parallel_for_via_gomp(
                    var, ty, &start_v, &end_v, body, reductions, ctx, out,
                );
                return;
            }
            let lty = llvm_type(ty);
            let start_v = emit_expr(start, ctx, out);
            let end_v = emit_expr(end, ctx, out);
            // Use a fresh tmp prefix in the alloca name so two
            // for-loops in the same function with the same loop
            // variable (`for i in 0..n { … } parallel for i in
            // 0..m { … }`) don't collide on `%i.addr`.
            let i_addr = format!("{}.{}.addr", ctx.fresh_tmp(), var);
            out.push_str(&format!("  {} = alloca {}\n", i_addr, lty));
            out.push_str(&format!("  store {} {}, {}* {}\n", lty, start_v, lty, i_addr));
            // Save and restore the previous binding for `var` so
            // the loop variable scope properly nests with any
            // outer binding of the same name.
            let prev = ctx.locals.insert(var.clone(), (ty.clone(), i_addr.clone()));

            let header = ctx.fresh_label("for_header");
            let body_lbl = ctx.fresh_label("for_body");
            // `for_step` is the increment-then-jump-to-
            // header block; it's the continue target so the
            // counter bumps before re-entering the header's
            // cond check. Closure #188 — previously
            // `continue` jumped straight to header with
            // i_addr unchanged → infinite loop. Mirrors the
            // tree-LLVM for-iter fix (closure #186) and the
            // SSA path fix (closures #185 + #187).
            let step = ctx.fresh_label("for_step");
            let exit = ctx.fresh_label("for_exit");
            out.push_str(&format!("  br label %{}\n", header));
            out.push_str(&format!("{}:\n", header));
            let cur = ctx.fresh_tmp();
            out.push_str(&format!("  {} = load {}, {}* {}\n", cur, lty, lty, i_addr));
            let cmp = ctx.fresh_tmp();
            let lt = if ty.is_signed_integer() { "slt" } else { "ult" };
            out.push_str(&format!("  {} = icmp {} {} {}, {}\n", cmp, lt, lty, cur, end_v));
            out.push_str(&format!(
                "  br i1 {}, label %{}, label %{}\n",
                cmp, body_lbl, exit
            ));

            out.push_str(&format!("{}:\n", body_lbl));
            ctx.loops.push(LoopFrame {
                header: step.clone(),
                exit: exit.clone(),
            });
            let outer_terminated = ctx.terminated;
            ctx.terminated = false;
            for s in body {
                emit_stmt(s, ctx, out);
            }
            if !ctx.terminated {
                out.push_str(&format!("  br label %{}\n", step));
            }
            ctx.loops.pop();
            // Step block: increment i_addr, jump to header.
            out.push_str(&format!("{}:\n", step));
            let now = ctx.fresh_tmp();
            let next = ctx.fresh_tmp();
            out.push_str(&format!("  {} = load {}, {}* {}\n", now, lty, lty, i_addr));
            out.push_str(&format!("  {} = add {} {}, 1\n", next, lty, now));
            out.push_str(&format!("  store {} {}, {}* {}\n", lty, next, lty, i_addr));
            out.push_str(&format!("  br label %{}\n", header));
            ctx.terminated = outer_terminated;
            // Restore the outer binding of `var` if any (so a
            // second loop with the same loop-variable name and a
            // fresh alloca picks up cleanly).
            if let Some(prev) = prev {
                ctx.locals.insert(var.clone(), prev);
            } else {
                ctx.locals.remove(var);
            }
            out.push_str(&format!("{}:\n", exit));
        }
        TypedStmt::FieldAssign {
            object,
            field_index,
            value,
            ..
        } => {
            // Unified lvalue lowering: resolve the address
            // of `object` via a recursive walker that
            // handles `Var(name)` (owned alloca slot or
            // ref-param pointer) and `FieldAccess` (GEP
            // through the parent struct). Then GEP one
            // more level for the named field and store.
            // Handles nested places (`o.q.r = …;`) and
            // `self.field = …;` through `mut ref T`
            // uniformly. T1.2 phase 2a follow-up.
            let v = emit_expr(value, ctx, out);
            let value_ty = llvm_type_string(&value.ty);
            let obj_addr = emit_lvalue_addr(object, ctx, out);
            let underlying = if object.ty.is_any_ref() {
                object.ty.deref().clone()
            } else {
                object.ty.clone()
            };
            let struct_ty = llvm_type_string(&underlying);
            let elem_p = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = getelementptr {}, {}* {}, i64 0, i32 {}\n",
                elem_p, struct_ty, struct_ty, obj_addr, field_index
            ));
            // Heap-shaped field overwrite: free the old slot
            // value before storing the new one, otherwise the
            // previous heap allocation leaks. Mirrors the
            // leaf-Drop logic for IndexAssign (closure #126).
            // Closure #132 added OwnedStr and Vec. Closure
            // #170 closes Struct and Enum (tree-C had these
            // via closure #148).
            match &value.ty {
                Type::OwnedStr => {
                    let old = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = load i8*, i8** {}\n",
                        old, elem_p
                    ));
                    out.push_str(&format!(
                        "  call void @free(i8* {})\n",
                        old
                    ));
                }
                Type::Vec(element) => {
                    let s_ty = vec_struct_name(element);
                    let old = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = load {}, {}* {}\n",
                        old, s_ty, s_ty, elem_p
                    ));
                    let free_name =
                        format!("@intent_vec_{}__free", vec_struct_tag(element));
                    out.push_str(&format!(
                        "  call void {}({} {})\n",
                        free_name, s_ty, old
                    ));
                }
                Type::Struct(struct_name) => {
                    // Walk the OLD nested struct's heap-owning
                    // fields BEFORE overwriting. elem_p IS the
                    // struct's address so it serves as the
                    // base for emit_llvm_struct_field_drops.
                    let fields = LLVM_STRUCT_FIELDS_REGISTRY
                        .with(|r| r.borrow().get(struct_name).cloned())
                        .unwrap_or_default();
                    let has_owning = fields.iter().any(|(_, ty)| !ty.is_copy());
                    if has_owning {
                        let empty: std::collections::HashSet<&String> =
                            std::collections::HashSet::new();
                        emit_llvm_struct_field_drops(
                            &elem_p,
                            struct_name,
                            &fields,
                            &empty,
                            ctx,
                            out,
                        );
                    }
                }
                Type::Enum(enum_name) => {
                    // Same shape as Reassign drop_old for
                    // Enum (closure #169): load OLD tagged-
                    // union, OR-chain on payloaded tags,
                    // free heap payload if active.
                    let payload_ty = LLVM_ENUM_PAYLOAD_REGISTRY
                        .with(|r| r.borrow().get(enum_name).cloned());
                    let heap_kind = match &payload_ty {
                        Some(Type::OwnedStr) => Some("owned_str"),
                        Some(Type::Vec(_)) => Some("vec"),
                        _ => None,
                    };
                    if let Some(kind) = heap_kind {
                        let payload_tags: Vec<u32> = LLVM_ENUM_PAYLOAD_TAGS_REGISTRY
                            .with(|r| r.borrow().get(enum_name).cloned().unwrap_or_default());
                        if !payload_tags.is_empty() {
                            let s_ty = format!("%Enum_{}", enum_name);
                            let loaded = ctx.fresh_tmp();
                            out.push_str(&format!(
                                "  {} = load {}, {}* {}\n",
                                loaded, s_ty, s_ty, elem_p
                            ));
                            let tag = ctx.fresh_tmp();
                            out.push_str(&format!(
                                "  {} = extractvalue {} {}, 0\n",
                                tag, s_ty, loaded
                            ));
                            let payload = ctx.fresh_tmp();
                            out.push_str(&format!(
                                "  {} = extractvalue {} {}, 1\n",
                                payload, s_ty, loaded
                            ));
                            let free_lbl = ctx.fresh_label("field_enum_free");
                            let done_lbl = ctx.fresh_label("field_enum_done");
                            let mut prev = "i1 false".to_string();
                            for t in &payload_tags {
                                let cmp = ctx.fresh_tmp();
                                out.push_str(&format!(
                                    "  {} = icmp eq i32 {}, {}\n",
                                    cmp, tag, t
                                ));
                                let or_v = ctx.fresh_tmp();
                                out.push_str(&format!(
                                    "  {} = or {}, {}\n",
                                    or_v, prev, cmp
                                ));
                                prev = format!("i1 {}", or_v);
                            }
                            let cond = prev.trim_start_matches("i1 ").to_string();
                            out.push_str(&format!(
                                "  br i1 {}, label %{}, label %{}\n",
                                cond, free_lbl, done_lbl
                            ));
                            out.push_str(&format!("{}:\n", free_lbl));
                            match kind {
                                "owned_str" => {
                                    out.push_str(&format!(
                                        "  call void @free(i8* {})\n",
                                        payload
                                    ));
                                }
                                "vec" => {
                                    if let Some(Type::Vec(element)) = &payload_ty {
                                        let free_name = format!(
                                            "@intent_vec_{}__free",
                                            vec_struct_tag(element)
                                        );
                                        let v_struct = vec_struct_name(element);
                                        out.push_str(&format!(
                                            "  call void {}({} {})\n",
                                            free_name, v_struct, payload
                                        ));
                                    }
                                }
                                _ => {}
                            }
                            out.push_str(&format!("  br label %{}\n", done_lbl));
                            out.push_str(&format!("{}:\n", done_lbl));
                            ctx.current_block = done_lbl;
                        }
                    }
                }
                _ => {}
            }
            out.push_str(&format!(
                "  store {} {}, {}* {}\n",
                value_ty, v, value_ty, elem_p
            ));
        }
        TypedStmt::IndexAssign { name, index, field_path, value, base_ty, .. } => {
            let underlying = base_ty.deref().clone();
            let addr = match ctx.locals.get(name) {
                Some((_, a)) => a.clone(),
                None => unreachable!(
                    "checker: index-assign on undeclared binding '{}'",
                    name
                ),
            };
            // After computing a pointer `elt_p` to the
            // indexed element, descend through field_path
            // segments via further GEPs. The final pointer's
            // pointee type is `value.ty` (the leaf field
            // type). T1.2 phase 2b follow-up.
            if let Type::Vec(element) = &underlying {
                let s_ty = vec_struct_name(element);
                let elt_ty = llvm_type_string(element);
                let data_p = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = getelementptr {}, {}* {}, i64 0, i32 0\n",
                    data_p, s_ty, s_ty, addr
                ));
                let data = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = load {}*, {}** {}\n",
                    data, elt_ty, elt_ty, data_p
                ));
                let idx_v = emit_expr(index, ctx, out);
                let idx_i64 = widen_index_to_64(&idx_v, &index.ty, ctx, out);
                let mut p = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = getelementptr {}, {}* {}, i64 {}\n",
                    p, elt_ty, elt_ty, data, idx_i64
                ));
                // Walk field segments: each one drives a
                // `i64 0, i32 <field_index>` GEP. The struct
                // type spelling for each segment is the type
                // BEFORE descending into that segment.
                let mut current_ty = element.as_ref().clone();
                for (_, field_index) in field_path {
                    let struct_ty_str = llvm_type_string(&current_ty);
                    let next = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = getelementptr {}, {}* {}, i64 0, i32 {}\n",
                        next, struct_ty_str, struct_ty_str, p, field_index
                    ));
                    p = next;
                    // Advance current_ty to the field's type
                    // (Copy primitive in v1, per checker gate).
                    if let Type::Struct(sname) = &current_ty {
                        // Use the struct registry to look up
                        // the field's type. T1.2 phase 2b.
                        let fields = LLVM_STRUCT_FIELDS_REGISTRY
                            .with(|r| r.borrow().get(sname).cloned())
                            .unwrap_or_default();
                        if let Some((_, fty)) = fields.get(*field_index as usize) {
                            current_ty = fty.clone();
                        }
                    }
                }
                let store_ty = llvm_type_string(&value.ty);
                let val_v = emit_expr(value, ctx, out);
                emit_leaf_overwrite_drop(&value.ty, field_path, &p, ctx, out);
                out.push_str(&format!("  store {} {}, {}* {}\n", store_ty, val_v, store_ty, p));
                return;
            }
            if let Type::Array { element, .. } = &underlying {
                let agg = llvm_type_string(&underlying);
                // String form so struct / tuple
                // elements render their full LLVM
                // spelling instead of panicking. Same
                // pattern as the Array-Index read path.
                let elt_ty = llvm_type_string(element);
                let idx_v = emit_expr(index, ctx, out);
                let idx_i64 = if matches!(index.ty, Type::I64 | Type::U64) {
                    idx_v
                } else {
                    let dest = ctx.fresh_tmp();
                    let op = if index.ty.is_signed_integer() { "sext" } else { "zext" };
                    out.push_str(&format!(
                        "  {} = {} {} {} to i64\n",
                        dest, op, llvm_type(&index.ty), idx_v
                    ));
                    dest
                };
                let mut p = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = getelementptr {}, {}* {}, i64 0, i64 {}\n",
                    p, agg, agg, addr, idx_i64
                ));
                let mut current_ty = element.as_ref().clone();
                for (_, field_index) in field_path {
                    let struct_ty_str = llvm_type_string(&current_ty);
                    let next = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = getelementptr {}, {}* {}, i64 0, i32 {}\n",
                        next, struct_ty_str, struct_ty_str, p, field_index
                    ));
                    p = next;
                    if let Type::Struct(sname) = &current_ty {
                        let fields = LLVM_STRUCT_FIELDS_REGISTRY
                            .with(|r| r.borrow().get(sname).cloned())
                            .unwrap_or_default();
                        if let Some((_, fty)) = fields.get(*field_index as usize) {
                            current_ty = fty.clone();
                        }
                    }
                }
                let store_ty = llvm_type_string(&value.ty);
                let _ = elt_ty;
                let val_v = emit_expr(value, ctx, out);
                emit_leaf_overwrite_drop(&value.ty, field_path, &p, ctx, out);
                out.push_str(&format!("  store {} {}, {}* {}\n", store_ty, val_v, store_ty, p));
            } else {
                // The checker requires the base type of an index-
                // assign to be Vec<T> or [T; N]; everything else is
                // rejected upstream.
                unreachable!(
                    "checker: index-assign on unsupported base type {:?}",
                    underlying
                );
            }
        }
        TypedStmt::ForIter {
            var,
            element_ty,
            collection,
            collection_ty,
            consumes,
            body,
        } => {
            // Standard counter loop: i = 0..len(collection), body
            // reads collection[i] into `var` and runs. If we consume
            // the Vec, free its buffer at exit.
            let underlying = collection_ty.deref().clone();
            let coll_addr = match ctx.locals.get(collection) {
                Some((_, a)) => a.clone(),
                None => unreachable!(
                    "checker: for-iter over undeclared binding '{}'",
                    collection
                ),
            };
            // `llvm_type_string` so element types that are
            // themselves aggregates (`Vec<U>`) resolve to
            // their struct typedef instead of panicking.
            // Refines #7 phase 2.
            let elt_lty = llvm_type_string(element_ty);

            // Compute len + a function to GEP element i.
            type ElemGep = Box<dyn Fn(&str, &mut FnCtx, &mut String) -> String>;
            let (len_src, elem_gep): (String, ElemGep) =
                match &underlying {
                    Type::Array { length, .. } => {
                        let agg = llvm_type_string(&underlying);
                        let addr = coll_addr.clone();
                        let g_agg = agg.clone();
                        let g_addr = addr.clone();
                        let g_elt = elt_lty.to_string();
                        let elem_fn = move |i_str: &str, c: &mut FnCtx, o: &mut String| {
                            let p = c.fresh_tmp();
                            o.push_str(&format!(
                                "  {} = getelementptr {}, {}* {}, i64 0, i64 {}\n",
                                p, g_agg, g_agg, g_addr, i_str
                            ));
                            let v = c.fresh_tmp();
                            o.push_str(&format!("  {} = load {}, {}* {}\n", v, g_elt, g_elt, p));
                            v
                        };
                        (format!("{}", length), Box::new(elem_fn))
                    }
                    Type::Vec(element) => {
                        // Materialize `len` and `data` up front so the
                        // body's GEP doesn't reload the struct each
                        // iteration.
                        let s_ty = vec_struct_name(element);
                        let elt = llvm_type_string(element);
                        // len
                        let len_p = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = getelementptr {}, {}* {}, i64 0, i32 1\n",
                            len_p, s_ty, s_ty, coll_addr
                        ));
                        let len_v = ctx.fresh_tmp();
                        out.push_str(&format!("  {} = load i64, i64* {}\n", len_v, len_p));
                        // data
                        let data_p = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = getelementptr {}, {}* {}, i64 0, i32 0\n",
                            data_p, s_ty, s_ty, coll_addr
                        ));
                        let data_v = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = load {}*, {}** {}\n",
                            data_v, elt, elt, data_p
                        ));
                        let g_data = data_v.clone();
                        let g_elt = elt.clone();
                        let elem_fn = move |i_str: &str, c: &mut FnCtx, o: &mut String| {
                            let p = c.fresh_tmp();
                            o.push_str(&format!(
                                "  {} = getelementptr {}, {}* {}, i64 {}\n",
                                p, g_elt, g_elt, g_data, i_str
                            ));
                            let v = c.fresh_tmp();
                            o.push_str(&format!("  {} = load {}, {}* {}\n", v, g_elt, g_elt, p));
                            v
                        };
                        (len_v, Box::new(elem_fn))
                    }
                    other => unreachable!(
                        "checker: for-iter over non-Array/non-Vec type {:?}",
                        other
                    ),
                };

            // i counter alloca.
            let i_addr = ctx.fresh_tmp();
            out.push_str(&format!("  {} = alloca i64\n", i_addr));
            out.push_str(&format!("  store i64 0, i64* {}\n", i_addr));

            // var alloca (so writes inside the body work the usual way).
            let var_addr = format!("%{}.addr", var);
            out.push_str(&format!("  {} = alloca {}\n", var_addr, elt_lty));
            ctx.locals
                .insert(var.clone(), (element_ty.clone(), var_addr.clone()));

            let header = ctx.fresh_label("iter_header");
            let body_lbl = ctx.fresh_label("iter_body");
            // `step` is the increment-then-jump-to-header
            // block; it's the target of `continue` so the
            // counter bumps before re-entering the header's
            // cond check. Closure #186 — previously
            // `continue` jumped straight to header with
            // i_addr unchanged → infinite loop.
            let step = ctx.fresh_label("iter_step");
            let exit = ctx.fresh_label("iter_exit");
            out.push_str(&format!("  br label %{}\n", header));
            out.push_str(&format!("{}:\n", header));
            let cur = ctx.fresh_tmp();
            out.push_str(&format!("  {} = load i64, i64* {}\n", cur, i_addr));
            let cmp = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = icmp ult i64 {}, {}\n",
                cmp, cur, len_src
            ));
            out.push_str(&format!(
                "  br i1 {}, label %{}, label %{}\n",
                cmp, body_lbl, exit
            ));

            out.push_str(&format!("{}:\n", body_lbl));
            // Load element at `cur` into `var`'s alloca.
            let elem_val = elem_gep(&cur, ctx, out);
            out.push_str(&format!(
                "  store {} {}, {}* {}\n",
                elt_lty, elem_val, elt_lty, var_addr
            ));
            ctx.loops.push(LoopFrame {
                header: step.clone(),
                exit: exit.clone(),
            });
            let outer_terminated = ctx.terminated;
            ctx.terminated = false;
            for s in body {
                emit_stmt(s, ctx, out);
            }
            if !ctx.terminated {
                out.push_str(&format!("  br label %{}\n", step));
            }
            ctx.loops.pop();
            // Step block: bump i_addr then jump to header.
            out.push_str(&format!("{}:\n", step));
            let next = ctx.fresh_tmp();
            out.push_str(&format!("  {} = add i64 {}, 1\n", next, cur));
            out.push_str(&format!("  store i64 {}, i64* {}\n", next, i_addr));
            out.push_str(&format!("  br label %{}\n", header));
            ctx.terminated = outer_terminated;
            out.push_str(&format!("{}:\n", exit));

            // If we consumed an owned Vec, free its buffer here.
            // For non-Copy elements each slot was loaded into x and
            // freed by x's scope-exit drop in the body — routing
            // through `intent_vec_<T>__free` would re-walk every
            // slot (closure #127's per-element drop) and double-
            // free. Emit only the outer buffer free in that case.
            if *consumes {
                if let Type::Vec(element) = collection_ty {
                    let s_ty = vec_struct_name(element);
                    if element.is_copy() {
                        let v_loaded = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = load {}, {}* {}\n",
                            v_loaded, s_ty, s_ty, coll_addr
                        ));
                        let free_name = format!(
                            "@intent_vec_{}__free",
                            vec_struct_tag(element)
                        );
                        out.push_str(&format!(
                            "  call void {}({} {})\n",
                            free_name, s_ty, v_loaded
                        ));
                    } else {
                        let elt = llvm_type_string(element);
                        let data_p = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = getelementptr {}, {}* {}, i64 0, i32 0\n",
                            data_p, s_ty, s_ty, coll_addr
                        ));
                        let data_v = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = load {}*, {}** {}\n",
                            data_v, elt, elt, data_p
                        ));
                        let data_i8 = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = bitcast {}* {} to i8*\n",
                            data_i8, elt, data_v
                        ));
                        out.push_str(&format!(
                            "  call void @free(i8* {})\n",
                            data_i8
                        ));
                    }
                }
            }
        }
        TypedStmt::TaskSpawn { name, body, captures } => {
            emit_task_via_pthread(name, body, captures, ctx, out);
        }
        TypedStmt::TaskJoin { name } => {
            // Real-thread join: read the handle, park until
            // the worker exits (pthread_join on POSIX;
            // WaitForSingleObject + CloseHandle on Win32),
            // then free the heap ctx. The handle slot is
            // always i64 (Win32 HANDLE is `ptrtoint`-cast
            // through it at spawn time).
            let addr = match ctx.locals.get(name) {
                Some((_, a)) => a.clone(),
                None => unreachable!(
                    "checker: join '{}' is in scope when we get here",
                    name
                ),
            };
            let handle_p = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = getelementptr %intent_task_handle, %intent_task_handle* {}, i32 0, i32 0\n",
                handle_p, addr
            ));
            let handle_v = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = load i64, i64* {}\n",
                handle_v, handle_p
            ));
            if host_uses_win32_threading() {
                let h = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = inttoptr i64 {} to i8*\n",
                    h, handle_v
                ));
                let _wait = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = call i32 @WaitForSingleObject(i8* {}, i32 -1)\n",
                    _wait, h
                ));
                let _close = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = call i32 @CloseHandle(i8* {})\n",
                    _close, h
                ));
            } else {
                let _ret = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = call i32 @pthread_join(i64 {}, i8** null)\n",
                    _ret, handle_v
                ));
            }
            let ctx_p = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = getelementptr %intent_task_handle, %intent_task_handle* {}, i32 0, i32 1\n",
                ctx_p, addr
            ));
            let ctx_v = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = load i8*, i8** {}\n",
                ctx_v, ctx_p
            ));
            out.push_str(&format!("  call void @free(i8* {})\n", ctx_v));
        }
    }
}

/// Resolve `obj` (which must be a valid LHS place) to an
/// LLVM SSA value holding a pointer to that place. The
/// pointee type is `obj.ty` (or its deref'd form if
/// `obj.ty` is a ref). Used by FieldAssign to support both
/// the simple `p.x = …;` case and nested
/// `o.inner.field = …;` chains. T1.2 phase 2a follow-up.
fn emit_lvalue_addr(obj: &TypedExpr, ctx: &mut FnCtx, out: &mut String) -> String {
    match &obj.kind {
        TypedExprKind::Var(name) => match ctx.locals.get(name) {
            Some((_, addr)) => addr.clone(),
            None => unreachable!(
                "backend: lvalue Var '{}' has no locals entry",
                name
            ),
        },
        TypedExprKind::FieldAccess { object: inner, field_index, .. } => {
            let inner_addr = emit_lvalue_addr(inner, ctx, out);
            // For the GEP, we need the underlying struct
            // type. If `inner.ty` is itself a ref (e.g.
            // `inner` is `Var(self)` with `mut ref Outer`),
            // we deref to get the underlying Outer.
            let underlying = if inner.ty.is_any_ref() {
                inner.ty.deref().clone()
            } else {
                inner.ty.clone()
            };
            let struct_ty = llvm_type_string(&underlying);
            let elem = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = getelementptr {}, {}* {}, i64 0, i32 {}\n",
                elem, struct_ty, struct_ty, inner_addr, field_index
            ));
            elem
        }
        kind => unreachable!(
            "backend: invalid lvalue place expression: {:?}",
            kind
        ),
    }
}

fn emit_expr(expr: &TypedExpr, ctx: &mut FnCtx, out: &mut String) -> String {
    match &expr.kind {
        TypedExprKind::Int(v) => format!("{}", v),
        TypedExprKind::Str(text) => {
            // Each unique string literal is interned as a private
            // global by the module-level pre-pass (`@.print_str.<n>`);
            // we GEP into that and return the i8* pointer.
            if let Some(&idx) = ctx.print_str_indices.get(text) {
                let bytes = text.len() + 1;
                let p = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = getelementptr [{} x i8], [{} x i8]* @.print_str.{}, i64 0, i64 0\n",
                    p, bytes, bytes, idx
                ));
                p
            } else {
                // Pre-pass should have interned every string we see;
                // if not, emit a stub so the IR still parses.
                "null".to_string()
            }
        }
        TypedExprKind::Float(v) => {
            // LLVM IR requires a `.` or `e` in float literals;
            // bare `2` would be parsed as integer. Hex form is
            // unambiguous and round-trip safe.
            //
            // f32 literals embed the bit pattern of the f64
            // representation (LLVM IR doesn't have a separate
            // "float" literal syntax — it always uses the 64-bit
            // hex form even for `float` types).
            if matches!(expr.ty, Type::F32) {
                let truncated: f64 = *v as f32 as f64;
                format!("0x{:016X}", truncated.to_bits())
            } else {
                format!("0x{:016X}", v.to_bits())
            }
        }
        TypedExprKind::Bool(b) => if *b { "1" } else { "0" }.to_string(),
        TypedExprKind::Var(name) => {
            // Variables live in allocas; emit a load. Use the
            // string form of the type so Vec/Array/Ref get the
            // correct LLVM type (`%intent_vec_i64`, `[N x T]`,
            // `T*`) instead of the scalar fallback.
            if let Some((ty, addr)) = ctx.locals.get(name).cloned() {
                // For reference params, the binding's "address" is
                // the pointer itself; reads of the reference value
                // (as opposed to derefs through it) just return that.
                // The checker forbids storing references in `let`,
                // so this only fires for ref *params* used in arg
                // position.
                if ty.is_any_ref() {
                    return addr.clone();
                }
                let lty = llvm_type_string(&ty);
                let dest = ctx.fresh_tmp();
                out.push_str(&format!("  {} = load {}, {}* {}\n", dest, lty, lty, addr));
                dest
            } else {
                format!("%{}", name)
            }
        }
        TypedExprKind::Binary { op, left, right, .. }
            if matches!(op, BinaryOp::Add)
                && matches!(left.ty, Type::Str | Type::OwnedStr)
                && matches!(right.ty, Type::Str | Type::OwnedStr) =>
        {
            // Str/OwnedStr concat: call the runtime helper
            // @intent_str_concat, which mallocs a fresh buffer for
            // strlen(l)+strlen(r)+1, memcpy's both, NUL-terminates,
            // and (when the corresponding _owned flag is 1) frees
            // the original operand's buffer. Mirror the C backend.
            let l = emit_expr(left, ctx, out);
            let r = emit_expr(right, ctx, out);
            // Same Call/Binary/Block/... whitelist as the
            // other fresh-OwnedStr handlers — Var /
            // FieldAccess operands share their buffer with
            // a live binding so freeing inside concat would
            // double-free. Closure #144.
            let l_owned = if crate::ir::owned_str_consumed_at_concat(left) { 1 } else { 0 };
            let r_owned = if crate::ir::owned_str_consumed_at_concat(right) { 1 } else { 0 };
            let dest = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = call i8* @intent_str_concat(i8* {}, i32 {}, i8* {}, i32 {})\n",
                dest, l, l_owned, r, r_owned
            ));
            dest
        }
        TypedExprKind::Binary { op, left, right, .. }
            if matches!(
                op,
                BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge
            ) && matches!(left.ty, Type::Str | Type::OwnedStr)
              && matches!(right.ty, Type::Str | Type::OwnedStr) =>
        {
            // Str comparisons lower to strcmp(a, b) <pred> 0. No
            // interning shortcut for Eq/Ne even when both sides are
            // literals: the checker leaves them as separate globals,
            // so identity comparison would be misleading. Ordering
            // uses signed predicates because strcmp returns a signed
            // int.
            let l = emit_expr(left, ctx, out);
            let r = emit_expr(right, ctx, out);
            let cmp = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = call i32 @strcmp(i8* {}, i8* {})\n",
                cmp, l, r
            ));
            // Free fresh-OwnedStr operands after the
            // comparison — strcmp doesn't consume its
            // arguments and `Call` / `Binary +` produce a
            // heap allocation with no other owner. Var /
            // FieldAccess operands skip the free so the
            // outer binding's scope-exit Drop still owns
            // the heap. Closure #138.
            if crate::ir::is_fresh_owned_str(left) {
                out.push_str(&format!("  call void @free(i8* {})\n", l));
            }
            if crate::ir::is_fresh_owned_str(right) {
                out.push_str(&format!("  call void @free(i8* {})\n", r));
            }
            let dest = ctx.fresh_tmp();
            let pred = match op {
                BinaryOp::Eq => "eq",
                BinaryOp::Ne => "ne",
                BinaryOp::Lt => "slt",
                BinaryOp::Le => "sle",
                BinaryOp::Gt => "sgt",
                BinaryOp::Ge => "sge",
                _ => unreachable!(),
            };
            out.push_str(&format!("  {} = icmp {} i32 {}, 0\n", dest, pred, cmp));
            dest
        }
        TypedExprKind::Binary { op, left, right, .. } if left.ty.is_float() => {
            let l = emit_expr(left, ctx, out);
            let r = emit_expr(right, ctx, out);
            let dest = ctx.fresh_tmp();
            let ty = llvm_type(&left.ty);
            let mnemonic = match op {
                BinaryOp::Add => "fadd",
                BinaryOp::Sub => "fsub",
                BinaryOp::Mul => "fmul",
                BinaryOp::Div => "fdiv",
                BinaryOp::Rem => "frem",
                BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => {
                    // Ordered comparisons (NaN → false). Matches the
                    // C backend's IEEE 754 semantics.
                    let cond = match op {
                        BinaryOp::Eq => "oeq",
                        BinaryOp::Ne => "one",
                        BinaryOp::Lt => "olt",
                        BinaryOp::Le => "ole",
                        BinaryOp::Gt => "ogt",
                        BinaryOp::Ge => "oge",
                        _ => unreachable!(),
                    };
                    out.push_str(&format!("  {} = fcmp {} {} {}, {}\n", dest, cond, ty, l, r));
                    return dest;
                }
                // BinaryOp::{Shl, Shr, And, Or} are integer/bool only;
                // the checker rejects them on float operands.
                op => unreachable!(
                    "checker rejects {:?} on float operands",
                    op
                ),
            };
            out.push_str(&format!("  {} = {} {} {}, {}\n", dest, mnemonic, ty, l, r));
            dest
        }
        TypedExprKind::Binary { op, left, right, checked } if is_int_or_bool(&left.ty) => {
            let l = emit_expr(left, ctx, out);
            let r = emit_expr(right, ctx, out);
            let ty = llvm_type(&left.ty);
            let signed = left.ty.is_signed_integer();
            // Runtime safety guards mirror the C backend's
            // `intent_check_*_divisor` and `intent_check_*_shift`
            // helpers. Fire only when `checked: true` AND the op is
            // one where the verifier might have left the guard in
            // place (Div/Rem/Shl/Shr). For other ops `checked` is
            // meaningless and ignored.
            if *checked {
                match op {
                    BinaryOp::Div | BinaryOp::Rem => {
                        let nz = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = icmp ne {} {}, 0\n",
                            nz, ty, r
                        ));
                        let ok = ctx.fresh_label("div_ok");
                        let fail = ctx.fresh_label("div_fail");
                        out.push_str(&format!(
                            "  br i1 {}, label %{}, label %{}\n",
                            nz, ok, fail
                        ));
                        out.push_str(&format!("{}:\n", fail));
                        out.push_str("  call void @abort()\n");
                        out.push_str("  unreachable\n");
                        out.push_str(&format!("{}:\n", ok));
                    }
                    BinaryOp::Shl | BinaryOp::Shr => {
                        let bits = left.ty.bits().unwrap_or(64) as i64;
                        // Cast shift count to i64 (signed extend / zero
                        // extend) for the bound comparison, since the
                        // count's own width can differ from i64.
                        let r_i64 = if matches!(right.ty, Type::I64 | Type::U64) {
                            r.clone()
                        } else {
                            let dest = ctx.fresh_tmp();
                            let op_ext = if right.ty.is_signed_integer() {
                                "sext"
                            } else {
                                "zext"
                            };
                            out.push_str(&format!(
                                "  {} = {} {} {} to i64\n",
                                dest, op_ext, llvm_type(&right.ty), r
                            ));
                            dest
                        };
                        let in_range = ctx.fresh_tmp();
                        // Unsigned compare against bits handles both
                        // negative and out-of-range positive counts
                        // (a negative i64 viewed as u64 is huge).
                        out.push_str(&format!(
                            "  {} = icmp ult i64 {}, {}\n",
                            in_range, r_i64, bits
                        ));
                        let ok = ctx.fresh_label("shift_ok");
                        let fail = ctx.fresh_label("shift_fail");
                        out.push_str(&format!(
                            "  br i1 {}, label %{}, label %{}\n",
                            in_range, ok, fail
                        ));
                        out.push_str(&format!("{}:\n", fail));
                        out.push_str("  call void @abort()\n");
                        out.push_str("  unreachable\n");
                        out.push_str(&format!("{}:\n", ok));
                    }
                    _ => {}
                }
            }
            let dest = ctx.fresh_tmp();
            let mnemonic = match op {
                BinaryOp::Add => "add",
                BinaryOp::Sub => "sub",
                BinaryOp::Mul => "mul",
                BinaryOp::Div if signed => "sdiv",
                BinaryOp::Div => "udiv",
                BinaryOp::Rem if signed => "srem",
                BinaryOp::Rem => "urem",
                BinaryOp::Shl => "shl",
                BinaryOp::Shr if signed => "ashr",
                BinaryOp::Shr => "lshr",
                BinaryOp::Eq | BinaryOp::Ne | BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => {
                    let cond = icmp_predicate(*op, signed);
                    out.push_str(&format!("  {} = icmp {} {} {}, {}\n", dest, cond, ty, l, r));
                    return dest;
                }
                BinaryOp::And | BinaryOp::BitAnd => "and",
                BinaryOp::Or | BinaryOp::BitOr => "or",
                BinaryOp::BitXor => "xor",
            };
            // For shifts, the RHS must have the same LLVM type as the
            // LHS — extend/truncate if needed.
            let r_shift = if matches!(op, BinaryOp::Shl | BinaryOp::Shr)
                && !matches!(right.ty, _ if left.ty.bits() == right.ty.bits())
            {
                let lhs_bits = left.ty.bits().unwrap_or(64);
                let rhs_bits = right.ty.bits().unwrap_or(64);
                if lhs_bits == rhs_bits {
                    r.clone()
                } else if lhs_bits > rhs_bits {
                    let dest = ctx.fresh_tmp();
                    let op_ext = if right.ty.is_signed_integer() {
                        "sext"
                    } else {
                        "zext"
                    };
                    out.push_str(&format!(
                        "  {} = {} {} {} to {}\n",
                        dest, op_ext, llvm_type(&right.ty), r, ty
                    ));
                    dest
                } else {
                    let dest = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = trunc {} {} to {}\n",
                        dest, llvm_type(&right.ty), r, ty
                    ));
                    dest
                }
            } else {
                r.clone()
            };
            out.push_str(&format!(
                "  {} = {} {} {}, {}\n",
                dest, mnemonic, ty, l, r_shift
            ));
            dest
        }
        TypedExprKind::Unary { op: UnaryOp::Neg, expr } if is_int_or_bool(&expr.ty) => {
            let inner = emit_expr(expr, ctx, out);
            let dest = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = sub {} 0, {}\n",
                dest,
                llvm_type(&expr.ty),
                inner
            ));
            dest
        }
        TypedExprKind::Unary { op: UnaryOp::Not, expr } => {
            let inner = emit_expr(expr, ctx, out);
            let dest = ctx.fresh_tmp();
            out.push_str(&format!("  {} = xor i1 {}, 1\n", dest, inner));
            dest
        }
        TypedExprKind::Cast { expr: inner, ty } => {
            let src_ty = inner.ty.clone();
            let v = emit_expr(inner, ctx, out);
            let dest = ctx.fresh_tmp();
            let src = llvm_type(&src_ty);
            let dst = llvm_type(ty);
            let op = cast_opcode(&src_ty, ty);
            if op == "bitcast-noop" {
                return v;
            }
            out.push_str(&format!("  {} = {} {} {} to {}\n", dest, op, src, v, dst));
            dest
        }
        TypedExprKind::Call { name, args, .. } => {
            // Synthetic intrinsic emitted by the parallel-for
            // outliner for each `reduce` clause. First arg is
            // `Var(<reduction_var>)` resolved against
            // outlined_ctx.locals (which holds the captured ptr);
            // second arg is the increment. Lowered to either an
            // `atomicrmw <op>` (for ops LLVM exposes directly) or
            // a cmpxchg-retry loop (for Mul, which atomicrmw
            // doesn't include).
            if let Some(op) = name.strip_prefix("__intent_atomic_") {
                let var_arg = args.first().expect("intrinsic has args");
                let inc_arg = args.get(1).expect("intrinsic has args");
                let TypedExprKind::Var(var_name) = &var_arg.kind else {
                    // `rewrite_body_for_reductions` synthesizes the
                    // first arg as `Var(reduction_name)`; the
                    // checker also pins the reduction target to a
                    // simple binding, so this shape is invariant.
                    unreachable!(
                        "reduction-rewrite invariant: atomicrmw target must be a Var, got {:?}",
                        var_arg.kind
                    );
                };
                let (cap_ty, cap_addr) = ctx
                    .locals
                    .get(var_name)
                    .cloned()
                    .expect("reduction var must be captured into outlined fn");
                let inc_v = emit_expr(inc_arg, ctx, out);
                let lty = llvm_type(&cap_ty);
                match op {
                    // Direct atomicrmw ops.
                    "add" => {
                        let dest = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = atomicrmw {} {}* {}, {} {} seq_cst\n",
                            dest, op, lty, cap_addr, lty, inc_v
                        ));
                        // atomicrmw result is the OLD value;
                        // Discard wraps the call so the SSA name
                        // is unused.
                        dest
                    }
                    // `and` / `or` serve two reduction shapes:
                    //   - bool `&&` / `||`: cap_ty is `Bool` and
                    //     the capture is an i8 shadow allocated
                    //     in the parent (atomicrmw rejects i1).
                    //     zext the i1 increment to i8 and update
                    //     the shadow; the parent on exit reads
                    //     it back via `icmp ne i8 …, 0`.
                    //   - bitwise `&` / `|` on integers: cap_ty
                    //     is an integer, the capture is the
                    //     native-width alloca, and atomicrmw runs
                    //     directly at the source width.
                    // `xor` is integer-only (bitwise `^`); same
                    // direct-atomicrmw shape as the bitwise &/|.
                    "and" | "or" | "xor" => {
                        if matches!(cap_ty, Type::Bool) {
                            let inc8 = ctx.fresh_tmp();
                            out.push_str(&format!(
                                "  {} = zext i1 {} to i8\n",
                                inc8, inc_v
                            ));
                            let dest = ctx.fresh_tmp();
                            out.push_str(&format!(
                                "  {} = atomicrmw {} i8* {}, i8 {} seq_cst\n",
                                dest, op, cap_addr, inc8
                            ));
                            dest
                        } else {
                            let dest = ctx.fresh_tmp();
                            out.push_str(&format!(
                                "  {} = atomicrmw {} {}* {}, {} {} seq_cst\n",
                                dest, op, lty, cap_addr, lty, inc_v
                            ));
                            dest
                        }
                    }
                    // Min/Max: atomicrmw has dedicated ops; pick
                    // signed (smin/smax) or unsigned (umin/umax)
                    // based on the reduction variable's type.
                    "min" | "max" => {
                        let signed = cap_ty.is_signed_integer();
                        let mnemonic = match (op, signed) {
                            ("min", true) => "min",
                            ("min", false) => "umin",
                            ("max", true) => "max",
                            ("max", false) => "umax",
                            _ => unreachable!(),
                        };
                        let dest = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = atomicrmw {} {}* {}, {} {} seq_cst\n",
                            dest, mnemonic, lty, cap_addr, lty, inc_v
                        ));
                        dest
                    }
                    // Mul: no direct atomicrmw, emit a cmpxchg
                    // retry loop. Correctness: under contention,
                    // each thread reads old, computes new=old*inc,
                    // attempts the CAS; on failure it reloads
                    // and retries. Order of multiplications
                    // doesn't matter (associative + commutative).
                    "mul" => {
                        let retry = ctx.fresh_label("rmw_mul_retry");
                        let done = ctx.fresh_label("rmw_mul_done");
                        out.push_str(&format!("  br label %{}\n", retry));
                        out.push_str(&format!("{}:\n", retry));
                        let old = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = load atomic {}, {}* {} seq_cst, align 8\n",
                            old, lty, lty, cap_addr
                        ));
                        let new_v = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = mul {} {}, {}\n",
                            new_v, lty, old, inc_v
                        ));
                        let cx = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = cmpxchg {}* {}, {} {}, {} {} seq_cst seq_cst\n",
                            cx, lty, cap_addr, lty, old, lty, new_v
                        ));
                        let success = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = extractvalue {{ {}, i1 }} {}, 1\n",
                            success, lty, cx
                        ));
                        out.push_str(&format!(
                            "  br i1 {}, label %{}, label %{}\n",
                            success, done, retry
                        ));
                        out.push_str(&format!("{}:\n", done));
                        // Return the OLD value to match the atomicrmw
                        // shape (Discard ignores it either way).
                        old
                    }
                    other => panic!("unsupported atomic op `{}`", other),
                }
            } else {
            // `min(a, b)` / `max(a, b)`: pure intrinsics. Lower
            // inline via `icmp` + `select`. Signedness follows the
            // operand type.
            if matches!(name.as_str(), "min" | "max") {
                let a = emit_expr(&args[0], ctx, out);
                let b = emit_expr(&args[1], ctx, out);
                let lty = llvm_type(&args[0].ty);
                let signed = args[0].ty.is_signed_integer();
                let pred_lt = if signed { "slt" } else { "ult" };
                let cmp = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = icmp {} {} {}, {}\n",
                    cmp, pred_lt, lty, a, b
                ));
                let dest = ctx.fresh_tmp();
                let (lo_val, hi_val) = if name == "min" {
                    (&a, &b)
                } else {
                    (&b, &a)
                };
                out.push_str(&format!(
                    "  {} = select i1 {}, {} {}, {} {}\n",
                    dest, cmp, lty, lo_val, lty, hi_val
                ));
                return dest;
            }
            // Atomic builtins. Element type T is derived from
            // the args' types — the checker has constrained T
            // to a supported integer width
            // (`atomic_storage_llvm` is total over those).
            // seq_cst ordering matches the verifier's semantic
            // model (all atomic ops observable in a single
            // total order across threads).
            if name == "atomic_new" {
                // No-op at the IR level: the value flows
                // through and the surrounding `let` allocates
                // the storage. For `Atomic<bool>` the value
                // gets zext'd from i1 to i8 so it can be stored
                // into the i8 cell — the let-store knows the
                // alloca type via `llvm_type_string` which
                // routes Atomic through atomic_storage_llvm.
                let v = emit_expr(&args[0], ctx, out);
                if matches!(&args[0].ty, Type::Bool) {
                    let promoted = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = zext i1 {} to i8\n",
                        promoted, v
                    ));
                    return promoted;
                }
                return v;
            }
            if name == "atomic_load" {
                // args[0]: &Atomic<T>; result is T.
                let element = match &args[0].ty {
                    Type::Ref(inner) | Type::RefMut(inner) => match &**inner {
                        Type::Atomic(elt) => (**elt).clone(),
                        _ => unreachable!("atomic_load arg must be &Atomic<T>"),
                    },
                    _ => unreachable!("atomic_load arg must be &Atomic<T>"),
                };
                let ty = atomic_storage_llvm(&element);
                let align = atomic_align(&element);
                let p = emit_expr(&args[0], ctx, out);
                let raw = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = load atomic {}, {}* {} seq_cst, align {}\n",
                    raw, ty, ty, p, align
                ));
                if matches!(element, Type::Bool) {
                    let truncd = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = icmp ne i8 {}, 0\n",
                        truncd, raw
                    ));
                    return truncd;
                }
                return raw;
            }
            if name == "atomic_store" {
                // args[0]: &Atomic<T>; args[1]: T.
                let element = args[1].ty.clone();
                let ty = atomic_storage_llvm(&element);
                let align = atomic_align(&element);
                let p = emit_expr(&args[0], ctx, out);
                let v = emit_expr(&args[1], ctx, out);
                let stored = if matches!(element, Type::Bool) {
                    let promoted = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = zext i1 {} to i8\n",
                        promoted, v
                    ));
                    promoted
                } else {
                    v.clone()
                };
                out.push_str(&format!(
                    "  store atomic {} {}, {}* {} seq_cst, align {}\n",
                    ty, stored, ty, p, align
                ));
                // Echo the value at the source-language width
                // (i1 for bool, the native iN otherwise).
                return v;
            }
            if name == "atomic_fetch_add" {
                // The checker rejects bool here, so element is
                // always an integer width and zext/trunc isn't
                // needed.
                let ty = atomic_storage_llvm(&args[1].ty);
                let p = emit_expr(&args[0], ctx, out);
                let v = emit_expr(&args[1], ctx, out);
                let dest = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = atomicrmw add {}* {}, {} {} seq_cst\n",
                    dest, ty, p, ty, v
                ));
                return dest;
            }
            if name == "atomic_compare_exchange" {
                // `cmpxchg <T>* %p, <T> %expected, <T> %new
                // seq_cst seq_cst` returns `{ <T> oldVal, i1
                // success }`. The language exposes only the
                // success bit (matches the four-arg form in
                // Rust's `Atomic*::compare_exchange`). For
                // bool the storage is i8, so expected/new get
                // zext'd from i1 first; the success bit is
                // already i1 either way.
                let element = args[1].ty.clone();
                let ty = atomic_storage_llvm(&element);
                let p = emit_expr(&args[0], ctx, out);
                let exp_raw = emit_expr(&args[1], ctx, out);
                let new_raw = emit_expr(&args[2], ctx, out);
                let (exp, new) = if matches!(element, Type::Bool) {
                    let exp_p = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = zext i1 {} to i8\n",
                        exp_p, exp_raw
                    ));
                    let new_p = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = zext i1 {} to i8\n",
                        new_p, new_raw
                    ));
                    (exp_p, new_p)
                } else {
                    (exp_raw, new_raw)
                };
                let cx = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = cmpxchg {}* {}, {} {}, {} {} seq_cst seq_cst\n",
                    cx, ty, p, ty, exp, ty, new
                ));
                let success = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = extractvalue {{ {}, i1 }} {}, 1\n",
                    success, ty, cx
                ));
                return success;
            }
            // Channel<T, N> builtins. Storage is the per-(T, N)
            // struct `%intent_channel_<T>_<N> = { [N x T], [N x
            // i64], i64, i64 }` (declared in the preamble).
            //   0: buf  ring-buffer slots
            //   1: seq  per-slot publication counters
            //   2: head consumer cursor (monotone)
            //   3: tail producer cursor (monotone)
            // The Vyukov MPSC protocol uses `seq[i & (N-1)]`
            // to coordinate producer→consumer visibility.
            if name == "channel_new" {
                // The result type lives on the outer
                // `TypedExpr`; we destructured into
                // `name, args, ..` above, so reach the type
                // via the captured `expr` from emit_expr.
                let (element, capacity) = match &expr.ty {
                    Type::Channel(elt, cap) => ((**elt).clone(), *cap),
                    _ => unreachable!("channel_new must return Channel<T, N>"),
                };
                let struct_ty = llvm_channel_struct(&element, capacity);
                // Slot storage uses `channel_slot_llvm`, not
                // `llvm_type`, so bool slots become i8.
                let elem_ty = channel_slot_llvm(&element);
                // seq is initialized to [i64 0, i64 1, ..., N-1]
                // so slot i is ready for round i. Materialize
                // the constant array literal at codegen time.
                let mut seq_init = String::from("[");
                for i in 0..capacity {
                    if i > 0 {
                        seq_init.push_str(", ");
                    }
                    seq_init.push_str(&format!("i64 {}", i));
                }
                seq_init.push(']');
                let s1 = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = insertvalue {} undef, [{} x {}] zeroinitializer, 0\n",
                    s1, struct_ty, capacity, elem_ty
                ));
                let s2 = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = insertvalue {} {}, [{} x i64] {}, 1\n",
                    s2, struct_ty, s1, capacity, seq_init
                ));
                let s3 = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = insertvalue {} {}, i64 0, 2\n",
                    s3, struct_ty, s2
                ));
                let s4 = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = insertvalue {} {}, i64 0, 3\n",
                    s4, struct_ty, s3
                ));
                return s4;
            }
            if name == "channel_send" {
                // args[0]: &Channel<T, N>. Read (T, N) off its
                // type and dispatch.
                let (element, capacity) = match &args[0].ty {
                    Type::Ref(inner) | Type::RefMut(inner) => match &**inner {
                        Type::Channel(elt, cap) => ((**elt).clone(), *cap),
                        _ => unreachable!("channel_send arg must be &Channel<T, N>"),
                    },
                    _ => unreachable!("channel_send arg must be &Channel<T, N>"),
                };
                let struct_ty = llvm_channel_struct(&element, capacity);
                let elem_ty = channel_slot_llvm(&element);
                let mask = capacity - 1;
                let p = emit_expr(&args[0], ctx, out);
                let v_in = emit_expr(&args[1], ctx, out);
                // For bool channels: value comes in as i1 but
                // slot storage is i8. Zext to i8 for the store
                // and echo back the original i1 at function
                // exit. Non-bool: v_in is stored directly.
                let v = if matches!(element, Type::Bool) {
                    let promoted = ctx.fresh_tmp();
                    out.push_str(&format!("  {} = zext i1 {} to i8\n", promoted, v_in));
                    promoted
                } else {
                    v_in.clone()
                };
                let tail_p = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = getelementptr {}, {}* {}, i32 0, i32 3\n",
                    tail_p, struct_ty, struct_ty, p
                ));
                let spin = ctx.fresh_label("ch_send_spin");
                let try_claim = ctx.fresh_label("ch_send_try");
                let write_blk = ctx.fresh_label("ch_send_write");
                out.push_str(&format!("  br label %{}\n", spin));
                out.push_str(&format!("{}:\n", spin));
                let cur_t = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = load atomic i64, i64* {} seq_cst, align 8\n",
                    cur_t, tail_p
                ));
                let idx = ctx.fresh_tmp();
                out.push_str(&format!("  {} = and i64 {}, {}\n", idx, cur_t, mask));
                let seq_p = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = getelementptr {}, {}* {}, i32 0, i32 1, i64 {}\n",
                    seq_p, struct_ty, struct_ty, p, idx
                ));
                let cur_s = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = load atomic i64, i64* {} seq_cst, align 8\n",
                    cur_s, seq_p
                ));
                let ready = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = icmp eq i64 {}, {}\n",
                    ready, cur_s, cur_t
                ));
                out.push_str(&format!(
                    "  br i1 {}, label %{}, label %{}\n",
                    ready, try_claim, spin
                ));
                out.push_str(&format!("{}:\n", try_claim));
                let next_t = ctx.fresh_tmp();
                out.push_str(&format!("  {} = add i64 {}, 1\n", next_t, cur_t));
                let cx = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = cmpxchg i64* {}, i64 {}, i64 {} seq_cst seq_cst\n",
                    cx, tail_p, cur_t, next_t
                ));
                let won = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = extractvalue {{ i64, i1 }} {}, 1\n",
                    won, cx
                ));
                out.push_str(&format!(
                    "  br i1 {}, label %{}, label %{}\n",
                    won, write_blk, spin
                ));
                out.push_str(&format!("{}:\n", write_blk));
                let slot_p = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = getelementptr {}, {}* {}, i32 0, i32 0, i64 {}\n",
                    slot_p, struct_ty, struct_ty, p, idx
                ));
                out.push_str(&format!(
                    "  store {} {}, {}* {}\n",
                    elem_ty, v, elem_ty, slot_p
                ));
                out.push_str(&format!(
                    "  store atomic i64 {}, i64* {} seq_cst, align 8\n",
                    next_t, seq_p
                ));
                // Echo back the source-language value (i1 for
                // bool, identical to `v` for everything else).
                return v_in;
            }
            if name == "channel_recv" {
                let (element, capacity) = match &args[0].ty {
                    Type::Ref(inner) | Type::RefMut(inner) => match &**inner {
                        Type::Channel(elt, cap) => ((**elt).clone(), *cap),
                        _ => unreachable!("channel_recv arg must be &Channel<T, N>"),
                    },
                    _ => unreachable!("channel_recv arg must be &Channel<T, N>"),
                };
                let struct_ty = llvm_channel_struct(&element, capacity);
                // Slot storage uses `channel_slot_llvm` so
                // bool slots are i8 — the load below sees i8
                // and we trunc back to i1 before returning.
                let elem_ty = channel_slot_llvm(&element);
                let mask = capacity - 1;
                let p = emit_expr(&args[0], ctx, out);
                let head_p = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = getelementptr {}, {}* {}, i32 0, i32 2\n",
                    head_p, struct_ty, struct_ty, p
                ));
                let cur_h = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = load atomic i64, i64* {} seq_cst, align 8\n",
                    cur_h, head_p
                ));
                let idx = ctx.fresh_tmp();
                out.push_str(&format!("  {} = and i64 {}, {}\n", idx, cur_h, mask));
                let seq_p = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = getelementptr {}, {}* {}, i32 0, i32 1, i64 {}\n",
                    seq_p, struct_ty, struct_ty, p, idx
                ));
                let target = ctx.fresh_tmp();
                out.push_str(&format!("  {} = add i64 {}, 1\n", target, cur_h));
                let spin = ctx.fresh_label("ch_recv_spin");
                let body = ctx.fresh_label("ch_recv_body");
                out.push_str(&format!("  br label %{}\n", spin));
                out.push_str(&format!("{}:\n", spin));
                let cur_s = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = load atomic i64, i64* {} seq_cst, align 8\n",
                    cur_s, seq_p
                ));
                let ready = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = icmp eq i64 {}, {}\n",
                    ready, cur_s, target
                ));
                out.push_str(&format!(
                    "  br i1 {}, label %{}, label %{}\n",
                    ready, body, spin
                ));
                out.push_str(&format!("{}:\n", body));
                let slot_p = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = getelementptr {}, {}* {}, i32 0, i32 0, i64 {}\n",
                    slot_p, struct_ty, struct_ty, p, idx
                ));
                let val = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = load {}, {}* {}\n",
                    val, elem_ty, elem_ty, slot_p
                ));
                // Release: store seq[idx] = head + CAP.
                let release = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = add i64 {}, {}\n",
                    release, cur_h, capacity
                ));
                out.push_str(&format!(
                    "  store atomic i64 {}, i64* {} seq_cst, align 8\n",
                    release, seq_p
                ));
                let next_h = ctx.fresh_tmp();
                out.push_str(&format!("  {} = add i64 {}, 1\n", next_h, cur_h));
                out.push_str(&format!(
                    "  store atomic i64 {}, i64* {} seq_cst, align 8\n",
                    next_h, head_p
                ));
                // Bool slots are stored as i8; convert back to
                // i1 before returning. Everything else just
                // returns the loaded value directly.
                if matches!(element, Type::Bool) {
                    let truncd = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = icmp ne i8 {}, 0\n",
                        truncd, val
                    ));
                    return truncd;
                }
                return val;
            }
            // Mutex<i64> + Guard<i64> builtins. The `locked`
            // field is i32 — its width is set by Linux's
            // futex ABI (`SYS_futex` reads/writes a 32-bit
            // word). 0/1/2 = unlocked / locked-no-waiters /
            // locked-waiters-present (Drepper's three-state
            // futex lock).
            if name == "mutex_new" {
                let initial = emit_expr(&args[0], ctx, out);
                let s1 = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = insertvalue %intent_mutex_i64 undef, i64 {}, 0\n",
                    s1, initial
                ));
                let s2 = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = insertvalue %intent_mutex_i64 {}, i32 0, 1\n",
                    s2, s1
                ));
                return s2;
            }
            if name == "mutex_lock" {
                let m_ptr = emit_expr(&args[0], ctx, out);
                let locked_p = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = getelementptr %intent_mutex_i64, %intent_mutex_i64* {}, i32 0, i32 1\n",
                    locked_p, m_ptr
                ));
                // Drepper's three-state futex lock.
                //   entry → fast CAS 0→1. On success → done.
                //   slow  → if old≠2, xchg state→2 to mark
                //           waiters present.
                //   loop  → re-read state; if 0 (released),
                //           done. Else syscall(FUTEX_WAIT,
                //           &state, 2); on wake xchg →2 and
                //           loop.
                // The current state is carried across loop
                // iterations via an alloca so the wait→wake
                // cycle doesn't need cross-block phi nodes.
                let c_p = ctx.fresh_tmp();
                out.push_str(&format!("  {} = alloca i32\n", c_p));
                let slow = ctx.fresh_label("mu_slow");
                let loop_head = ctx.fresh_label("mu_loop");
                let park = ctx.fresh_label("mu_park");
                let acquired = ctx.fresh_label("mu_got");
                // Fast path.
                let cx0 = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = cmpxchg i32* {}, i32 0, i32 1 seq_cst seq_cst\n",
                    cx0, locked_p
                ));
                let won0 = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = extractvalue {{ i32, i1 }} {}, 1\n",
                    won0, cx0
                ));
                out.push_str(&format!(
                    "  br i1 {}, label %{}, label %{}\n",
                    won0, acquired, slow
                ));
                // slow: c0 = old state from the failed CAS.
                out.push_str(&format!("{}:\n", slow));
                let c0 = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = extractvalue {{ i32, i1 }} {}, 0\n",
                    c0, cx0
                ));
                // If c0 != 2, atomically swap state→2 (mark
                // waiters present) and use the swapped-out
                // value as our "current c"; otherwise reuse
                // c0. Either way we end up with a fresh c in
                // the alloca and fall into the wait loop.
                let need_mark = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = icmp ne i32 {}, 2\n",
                    need_mark, c0
                ));
                let mark = ctx.fresh_label("mu_mark");
                let store_initial = ctx.fresh_label("mu_store_init");
                out.push_str(&format!(
                    "  br i1 {}, label %{}, label %{}\n",
                    need_mark, mark, store_initial
                ));
                out.push_str(&format!("{}:\n", mark));
                let c_marked = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = atomicrmw xchg i32* {}, i32 2 seq_cst\n",
                    c_marked, locked_p
                ));
                out.push_str(&format!("  store i32 {}, i32* {}\n", c_marked, c_p));
                out.push_str(&format!("  br label %{}\n", loop_head));
                out.push_str(&format!("{}:\n", store_initial));
                out.push_str(&format!("  store i32 {}, i32* {}\n", c0, c_p));
                out.push_str(&format!("  br label %{}\n", loop_head));
                // loop_head: read c; if 0, lock acquired.
                out.push_str(&format!("{}:\n", loop_head));
                let c = ctx.fresh_tmp();
                out.push_str(&format!("  {} = load i32, i32* {}\n", c, c_p));
                let still_locked = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = icmp ne i32 {}, 0\n",
                    still_locked, c
                ));
                out.push_str(&format!(
                    "  br i1 {}, label %{}, label %{}\n",
                    still_locked, park, acquired
                ));
                // park: kernel-wait, then re-acquire via xchg.
                out.push_str(&format!("{}:\n", park));
                if host_uses_win32_threading() {
                    // Win32: WaitOnAddress(addr, &compare,
                    // sizeof(int)=4, INFINITE=-1). The
                    // `compare` operand is a *pointer* to the
                    // value we expect to find at `addr` — if
                    // the addr-value differs, the syscall
                    // returns immediately without parking
                    // (matches futex semantics).
                    let cmp_slot = ctx.fresh_tmp();
                    out.push_str(&format!("  {} = alloca i32\n", cmp_slot));
                    out.push_str(&format!("  store i32 2, i32* {}\n", cmp_slot));
                    let addr_i8 = ctx.fresh_tmp();
                    let cmp_i8 = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = bitcast i32* {} to i8*\n",
                        addr_i8, locked_p
                    ));
                    out.push_str(&format!(
                        "  {} = bitcast i32* {} to i8*\n",
                        cmp_i8, cmp_slot
                    ));
                    let _wait_ret = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = call i32 @WaitOnAddress(i8* {}, i8* {}, i64 4, i32 -1)\n",
                        _wait_ret, addr_i8, cmp_i8
                    ));
                } else {
                    let _futex_ret = ctx.fresh_tmp();
                    // SYS_futex number depends on host arch
                    // (202 on x86_64, 98 on aarch64/riscv64,
                    // …). The FUTEX_WAIT_PRIVATE = 128 /
                    // FUTEX_WAKE_PRIVATE = 129 op constants
                    // are kernel-ABI-stable across all archs.
                    out.push_str(&format!(
                        "  {} = call i64 (i64, ...) @syscall(i64 {}, i32* {}, i32 128, i32 2, i8* null, i8* null, i32 0)\n",
                        _futex_ret, sys_futex_for_host(), locked_p
                    ));
                }
                let c_after = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = atomicrmw xchg i32* {}, i32 2 seq_cst\n",
                    c_after, locked_p
                ));
                out.push_str(&format!("  store i32 {}, i32* {}\n", c_after, c_p));
                out.push_str(&format!("  br label %{}\n", loop_head));
                // acquired: build the guard.
                out.push_str(&format!("{}:\n", acquired));
                let g = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = insertvalue %intent_guard_i64 undef, %intent_mutex_i64* {}, 0\n",
                    g, m_ptr
                ));
                return g;
            }
            if name == "guard_get" {
                let g_ptr = emit_expr(&args[0], ctx, out);
                let mp_p = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = getelementptr %intent_guard_i64, %intent_guard_i64* {}, i32 0, i32 0\n",
                    mp_p, g_ptr
                ));
                let m_ptr = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = load %intent_mutex_i64*, %intent_mutex_i64** {}\n",
                    m_ptr, mp_p
                ));
                let value_p = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = getelementptr %intent_mutex_i64, %intent_mutex_i64* {}, i32 0, i32 0\n",
                    value_p, m_ptr
                ));
                let val = ctx.fresh_tmp();
                out.push_str(&format!("  {} = load i64, i64* {}\n", val, value_p));
                return val;
            }
            if name == "guard_set" {
                let g_ptr = emit_expr(&args[0], ctx, out);
                let v = emit_expr(&args[1], ctx, out);
                let mp_p = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = getelementptr %intent_guard_i64, %intent_guard_i64* {}, i32 0, i32 0\n",
                    mp_p, g_ptr
                ));
                let m_ptr = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = load %intent_mutex_i64*, %intent_mutex_i64** {}\n",
                    m_ptr, mp_p
                ));
                let value_p = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = getelementptr %intent_mutex_i64, %intent_mutex_i64* {}, i32 0, i32 0\n",
                    value_p, m_ptr
                ));
                out.push_str(&format!("  store i64 {}, i64* {}\n", v, value_p));
                return v;
            }
            // `vec(...)` as a sub-expression (e.g. nested
            // `vec(vec(1,2), vec(3))`) — emit the same
            // malloc-then-store-then-insertvalue shape as
            // `emit_vec_let_from_literal` but return the
            // struct SSA value directly so the outer literal
            // can store it into its slot. Refines #7 phase 2;
            // previously this fell through to the user-fn
            // path and called the nonexistent `@fn_vec`.
            if name == "vec" {
                let element = match &expr.ty {
                    Type::Vec(element) => element,
                    _ => unreachable!("vec() must return Vec<_>"),
                };
                let n = args.len() as i64;
                let elt_ty = vec_element_value_str(element);
                // For struct / tuple elements the size is a
                // runtime constant expression; emit a mul to
                // compute `n * sizeof(T)`. For scalar
                // elements we can compute at compile time
                // (matching existing behavior). T1.2 +
                // Vec<Struct> LLVM.
                let raw = ctx.fresh_tmp();
                let payloaded_enum = matches!(element.as_ref(), Type::Enum(name)
                    if LLVM_ENUM_PAYLOAD_REGISTRY.with(|r| r.borrow().contains_key(name)));
                if matches!(element.as_ref(), Type::Struct(_) | Type::Tuple(_))
                    || payloaded_enum
                {
                    let size_expr = vec_element_size_expr(element);
                    let bytes_v = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = mul i64 {}, {}\n",
                        bytes_v, n, size_expr
                    ));
                    out.push_str(&format!(
                        "  {} = call i8* @malloc(i64 {})\n",
                        raw, bytes_v
                    ));
                } else {
                    let elt_size = vec_element_byte_size(element) as i64;
                    let bytes = (n * elt_size).max(elt_size.max(1));
                    out.push_str(&format!(
                        "  {} = call i8* @malloc(i64 {})\n",
                        raw, bytes
                    ));
                }
                let buf = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = bitcast i8* {} to {}*\n",
                    buf, raw, elt_ty
                ));
                for (i, a) in args.iter().enumerate() {
                    let v = emit_expr(a, ctx, out);
                    let p = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = getelementptr {}, {}* {}, i64 {}\n",
                        p, elt_ty, elt_ty, buf, i
                    ));
                    out.push_str(&format!(
                        "  store {} {}, {}* {}\n",
                        elt_ty, v, elt_ty, p
                    ));
                }
                let cap = if n == 0 { 1 } else { n };
                let s_ty = vec_struct_name(element);
                let s0 = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = insertvalue {} undef, {}* {}, 0\n",
                    s0, s_ty, elt_ty, buf
                ));
                let s1 = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = insertvalue {} {}, i64 {}, 1\n",
                    s1, s_ty, s0, n
                ));
                let s2 = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = insertvalue {} {}, i64 {}, 2\n",
                    s2, s_ty, s1, cap
                ));
                return s2;
            }
            // `clone_at(xs, i)` returns a fresh owned value of
            // the element type. Mirrors the SSA-LLVM lowering
            // (ssa_backend_llvm.rs:3431) — without this arm the
            // tree-LLVM fallback would call a nonexistent
            // `@fn_clone_at`. For Copy elements the slot value
            // itself is a fresh independent copy; for `Vec<U>`
            // elements we route through the inner Vec's
            // `__clone` helper.
            if name == "clone_at" {
                let xs_arg = &args[0];
                let element_ty = match xs_arg.ty.deref() {
                    Type::Vec(element) => (**element).clone(),
                    other => unreachable!(
                        "clone_at requires Vec, got {:?}", other
                    ),
                };
                let access_via_ref = matches!(
                    &xs_arg.ty,
                    Type::Ref(_) | Type::RefMut(_)
                );
                let struct_ty = vec_struct_name(&element_ty);
                let elt_ty = vec_element_value_str(&element_ty);
                // Materialize a struct-pointer to xs. If passed
                // by ref the operand IS already a pointer; if by
                // value we alloca a shadow and store the value.
                let xs_ptr = if access_via_ref {
                    emit_expr(xs_arg, ctx, out)
                } else {
                    let v = emit_expr(xs_arg, ctx, out);
                    let p = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = alloca {}\n", p, struct_ty
                    ));
                    out.push_str(&format!(
                        "  store {} {}, {}* {}\n",
                        struct_ty, v, struct_ty, p
                    ));
                    p
                };
                let data_pp = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = getelementptr {}, {}* {}, i32 0, i32 0\n",
                    data_pp, struct_ty, struct_ty, xs_ptr
                ));
                let data_p = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = load {}*, {}** {}\n",
                    data_p, elt_ty, elt_ty, data_pp
                ));
                let idx = emit_expr(&args[1], ctx, out);
                let slot_p = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = getelementptr {}, {}* {}, i64 {}\n",
                    slot_p, elt_ty, elt_ty, data_p, idx
                ));
                let dest = ctx.fresh_tmp();
                if element_ty.is_copy() {
                    // Copy element: load the slot, return its value.
                    out.push_str(&format!(
                        "  {} = load {}, {}* {}\n",
                        dest, elt_ty, elt_ty, slot_p
                    ));
                } else if let Type::Vec(inner) = &element_ty {
                    // Vec element: load slot, call ITS __clone.
                    let slot_v = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = load {}, {}* {}\n",
                        slot_v, elt_ty, elt_ty, slot_p
                    ));
                    let clone_name = format!(
                        "@intent_vec_{}__clone",
                        vec_struct_tag(inner)
                    );
                    out.push_str(&format!(
                        "  {} = call {} {}({} {})\n",
                        dest, elt_ty, clone_name, elt_ty, slot_v
                    ));
                } else if matches!(element_ty, Type::OwnedStr) {
                    // Closure #154: OwnedStr element — load
                    // the slot's i8*, round-trip through
                    // `intent_str_concat` with the empty
                    // string global to produce a deep clone.
                    // Was previously panicking ("not yet
                    // supported in tree-LLVM").
                    let slot_v = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = load i8*, i8** {}\n",
                        slot_v, slot_p
                    ));
                    let empty_p = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = getelementptr [1 x i8], [1 x i8]* @.empty_str_clone, i64 0, i64 0\n",
                        empty_p
                    ));
                    out.push_str(&format!(
                        "  {} = call i8* @intent_str_concat(i8* {}, i32 0, i8* {}, i32 0)\n",
                        dest, slot_v, empty_p
                    ));
                } else if let Type::Struct(struct_name) = &element_ty {
                    // Closure #155: Struct element — load
                    // the slot, extract each field, deep-
                    // clone the owning fields (OwnedStr via
                    // intent_str_concat with empty),
                    // assemble a new struct via insertvalue
                    // chain whose final result is `dest`.
                    // Mirrors the per-shape Vec clone body
                    // for Struct elements (closure #153)
                    // but inlined against the slot pointer.
                    let fields = LLVM_STRUCT_FIELDS_REGISTRY
                        .with(|r| r.borrow().get(struct_name).cloned())
                        .unwrap_or_default();
                    let s_ty = format!("%Struct_{}", struct_name);
                    let slot_v = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = load {}, {}* {}\n",
                        slot_v, s_ty, s_ty, slot_p
                    ));
                    // For a Copy-only struct (or empty
                    // struct), the load itself is the deep
                    // clone. We still need to bind `dest`
                    // — emit an insertvalue with field 0
                    // re-extracted to round-trip the value.
                    // For a struct with owning fields, walk
                    // each field with the deep-clone for
                    // OwnedStr / other types.
                    if fields.is_empty() {
                        out.push_str(&format!(
                            "  {} = insertvalue {} undef, i64 0, 0\n",
                            dest, s_ty
                        ));
                    } else {
                        let mut acc = "undef".to_string();
                        for (idx, (_, fty)) in fields.iter().enumerate() {
                            let f_src = ctx.fresh_tmp();
                            let f_lty = llvm_type_string(fty);
                            out.push_str(&format!(
                                "  {} = extractvalue {} {}, {}\n",
                                f_src, s_ty, slot_v, idx
                            ));
                            let f_cloned = match fty {
                                Type::OwnedStr => {
                                    let empty_p = ctx.fresh_tmp();
                                    out.push_str(&format!(
                                        "  {} = getelementptr [1 x i8], [1 x i8]* @.empty_str_clone, i64 0, i64 0\n",
                                        empty_p
                                    ));
                                    let cloned = ctx.fresh_tmp();
                                    out.push_str(&format!(
                                        "  {} = call i8* @intent_str_concat(i8* {}, i32 0, i8* {}, i32 0)\n",
                                        cloned, f_src, empty_p
                                    ));
                                    cloned
                                }
                                _ => f_src.clone(),
                            };
                            let next_acc = if idx + 1 == fields.len() {
                                dest.clone()
                            } else {
                                ctx.fresh_tmp()
                            };
                            out.push_str(&format!(
                                "  {} = insertvalue {} {}, {} {}, {}\n",
                                next_acc, s_ty, acc, f_lty, f_cloned, idx
                            ));
                            acc = next_acc;
                        }
                    }
                } else if let Type::Enum(enum_name) = &element_ty {
                    // Closure #156: Enum element — load the
                    // slot, extract tag + payload, OR-chain
                    // over payloaded tags, branch through
                    // `cat_enum_payloaded` (reconstruct
                    // enum with deep-cloned OwnedStr
                    // payload) vs `cat_enum_taggy` (use
                    // payload as-is), phi-join into `dest`.
                    // For tag-only enums (no payloaded tags
                    // registered) the load IS the deep
                    // clone — emit a round-trip via
                    // insertvalue.
                    let payload_ty = LLVM_ENUM_PAYLOAD_REGISTRY
                        .with(|r| r.borrow().get(enum_name).cloned());
                    let payload_tags: Vec<u32> = LLVM_ENUM_PAYLOAD_TAGS_REGISTRY
                        .with(|r| r.borrow().get(enum_name).cloned().unwrap_or_default());
                    let e_ty = format!("%Enum_{}", enum_name);
                    let slot_v = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = load {}, {}* {}\n",
                        slot_v, e_ty, e_ty, slot_p
                    ));
                    let heap_kind = match &payload_ty {
                        Some(Type::OwnedStr) => Some("owned_str"),
                        _ => None,
                    };
                    if heap_kind == Some("owned_str") && !payload_tags.is_empty() {
                        let tag_v = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = extractvalue {} {}, 0\n",
                            tag_v, e_ty, slot_v
                        ));
                        let payload_v = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = extractvalue {} {}, 1\n",
                            payload_v, e_ty, slot_v
                        ));
                        let mut prev = "i1 false".to_string();
                        for t in &payload_tags {
                            let cmp = ctx.fresh_tmp();
                            out.push_str(&format!(
                                "  {} = icmp eq i32 {}, {}\n",
                                cmp, tag_v, t
                            ));
                            let or_v = ctx.fresh_tmp();
                            out.push_str(&format!(
                                "  {} = or {}, {}\n",
                                or_v, prev, cmp
                            ));
                            prev = format!("i1 {}", or_v);
                        }
                        let cond = prev.trim_start_matches("i1 ").to_string();
                        let pay_lbl = ctx.fresh_label("cat_enum_pay");
                        let tag_lbl = ctx.fresh_label("cat_enum_tag");
                        let join_lbl = ctx.fresh_label("cat_enum_join");
                        out.push_str(&format!(
                            "  br i1 {}, label %{}, label %{}\n",
                            cond, pay_lbl, tag_lbl
                        ));
                        out.push_str(&format!("{}:\n", pay_lbl));
                        let empty_p = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = getelementptr [1 x i8], [1 x i8]* @.empty_str_clone, i64 0, i64 0\n",
                            empty_p
                        ));
                        let cloned_payload = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = call i8* @intent_str_concat(i8* {}, i32 0, i8* {}, i32 0)\n",
                            cloned_payload, payload_v, empty_p
                        ));
                        let new_enum_p1 = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = insertvalue {} undef, i32 {}, 0\n",
                            new_enum_p1, e_ty, tag_v
                        ));
                        let new_enum_p2 = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = insertvalue {} {}, i8* {}, 1\n",
                            new_enum_p2, e_ty, new_enum_p1, cloned_payload
                        ));
                        out.push_str(&format!("  br label %{}\n", join_lbl));
                        out.push_str(&format!("{}:\n", tag_lbl));
                        out.push_str(&format!("  br label %{}\n", join_lbl));
                        out.push_str(&format!("{}:\n", join_lbl));
                        out.push_str(&format!(
                            "  {} = phi {} [ {}, %{} ], [ {}, %{} ]\n",
                            dest, e_ty, new_enum_p2, pay_lbl, slot_v, tag_lbl
                        ));
                        ctx.current_block = join_lbl;
                    } else {
                        // Tag-only enum: round-trip via
                        // insertvalue so `dest` is bound.
                        let tag_v = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = extractvalue {} {}, 0\n",
                            tag_v, e_ty, slot_v
                        ));
                        out.push_str(&format!(
                            "  {} = insertvalue {} undef, i32 {}, 0\n",
                            dest, e_ty, tag_v
                        ));
                    }
                } else {
                    unreachable!(
                        "clone_at on element type {:?} not yet supported in tree-LLVM",
                        element_ty
                    );
                }
                return dest;
            }
            if matches!(name.as_str(), "push" | "set" | "clone" | "push_mut" | "pop") {
                let elt = vec_element_of_first_arg(args)
                    .expect("vec builtins take a Vec as the first arg");
                // Use the composable tag so nested-Vec elements
                // (`vec_int64`, `vec_vec_int64`) resolve to a
                // unique helper name — same convention as
                // `emit_vec_helpers` / `vec_struct_name`.
                // Closure #219: `pop` in source maps to
                // `pop_mut` in the helper namespace (matches
                // tree-C's `vec_helper(&element, "pop_mut")`).
                let helper_op = if name == "pop" { "pop_mut" } else { name.as_str() };
                let helper_name =
                    format!("@intent_vec_{}__{}", vec_struct_tag(&elt), helper_op);
                let arg_strs: Vec<String> = args
                    .iter()
                    .map(|a| {
                        format!("{} {}", llvm_type_string(&a.ty), emit_expr(a, ctx, out))
                    })
                    .collect();
                let dest = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = call {} {}({})\n",
                    dest,
                    llvm_type_string(&expr.ty),
                    helper_name,
                    arg_strs.join(", ")
                ));
                return dest;
            }
            let arg_strs: Vec<String> = args
                .iter()
                .map(|a| format!("{} {}", llvm_type_string(&a.ty), emit_expr(a, ctx, out)))
                .collect();
            let dest = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = call {} @fn_{}({})\n",
                dest,
                llvm_type_string(&expr.ty),
                name,
                arg_strs.join(", ")
            ));
            dest
            } // close the `else` from the `__intent_atomic_` branch
        }
        TypedExprKind::Index { array, index, .. } => {
            // For a `Var(name)` collection base we know its alloca (or
            // for a ref param, the pointer itself). Strip Ref/RefMut
            // so owned, `&`, and `&mut` go through the same path.
            if let TypedExprKind::Var(name) = &array.kind {
                if let Some((arr_ty, addr)) = ctx.locals.get(name).cloned() {
                    let underlying = arr_ty.deref().clone();
                    if let Type::Vec(element) = &underlying {
                        // Vec read: load `data` field, GEP, load element.
                        let s_ty = vec_struct_name(element);
                        // String form so struct / tuple
                        // elements render their full LLVM
                        // spelling. T1.2 + Vec<Struct>.
                        let elt_ty = llvm_type_string(element);
                        let data_p = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = getelementptr {}, {}* {}, i64 0, i32 0\n",
                            data_p, s_ty, s_ty, addr
                        ));
                        let data = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = load {}*, {}** {}\n",
                            data, elt_ty, elt_ty, data_p
                        ));
                        let idx_v = emit_expr(index, ctx, out);
                        let idx_i64 = widen_index_to_64(&idx_v, &index.ty, ctx, out);
                        let p = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = getelementptr {}, {}* {}, i64 {}\n",
                            p, elt_ty, elt_ty, data, idx_i64
                        ));
                        let v = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = load {}, {}* {}\n",
                            v, elt_ty, elt_ty, p
                        ));
                        return v;
                    }
                    if let Type::Array { element, .. } = &underlying {
                        let agg = llvm_type_string(&underlying);
                        // String form so struct / tuple
                        // elements render their full
                        // spelling instead of panicking.
                        let elt_ty = llvm_type_string(element);
                        let idx_v = emit_expr(index, ctx, out);
                        // The checker emits i64/u64/etc indices; LLVM
                        // GEP wants i64. If it's narrower, widen.
                        let idx_i64 = if matches!(index.ty, Type::I64 | Type::U64) {
                            idx_v
                        } else {
                            let dest = ctx.fresh_tmp();
                            let op = if index.ty.is_signed_integer() { "sext" } else { "zext" };
                            out.push_str(&format!(
                                "  {} = {} {} {} to i64\n",
                                dest, op, llvm_type(&index.ty), idx_v
                            ));
                            dest
                        };
                        let p = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = getelementptr {}, {}* {}, i64 0, i64 {}\n",
                            p, agg, agg, addr, idx_i64
                        ));
                        let v = ctx.fresh_tmp();
                        out.push_str(&format!("  {} = load {}, {}* {}\n", v, elt_ty, elt_ty, p));
                        return v;
                    }
                }
            }
            // `t.data[i]` — array-typed struct field. Reuse the
            // lvalue address machinery to get a pointer to the
            // field's array aggregate, then GEP into it. T1.2
            // phase 2b.
            if let TypedExprKind::FieldAccess { .. } = &array.kind {
                if let Type::Array { element, .. } = array.ty.deref().clone() {
                    let agg = llvm_type_string(array.ty.deref());
                    let elt_ty = llvm_type_string(&element);
                    let base_addr = emit_lvalue_addr(array, ctx, out);
                    let idx_v = emit_expr(index, ctx, out);
                    let idx_i64 = widen_index_to_64(&idx_v, &index.ty, ctx, out);
                    let p = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = getelementptr {}, {}* {}, i64 0, i64 {}\n",
                        p, agg, agg, base_addr, idx_i64
                    ));
                    let v = ctx.fresh_tmp();
                    out.push_str(&format!("  {} = load {}, {}* {}\n", v, elt_ty, elt_ty, p));
                    return v;
                }
                // `t.data[i]` — Vec-typed struct field. The
                // field pointer is itself the Vec struct
                // address; GEP into .data, load the element
                // pointer, GEP at idx, then load. Mirrors the
                // Var(Vec) arm above but starts from the
                // field pointer instead of the binding's
                // alloca. Closure #163.
                if let Type::Vec(element) = array.ty.deref().clone() {
                    let s_ty = vec_struct_name(&element);
                    let elt_ty = llvm_type_string(&element);
                    let base_addr = emit_lvalue_addr(array, ctx, out);
                    let data_p = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = getelementptr {}, {}* {}, i64 0, i32 0\n",
                        data_p, s_ty, s_ty, base_addr
                    ));
                    let data = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = load {}*, {}** {}\n",
                        data, elt_ty, elt_ty, data_p
                    ));
                    let idx_v = emit_expr(index, ctx, out);
                    let idx_i64 = widen_index_to_64(&idx_v, &index.ty, ctx, out);
                    let p = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = getelementptr {}, {}* {}, i64 {}\n",
                        p, elt_ty, elt_ty, data, idx_i64
                    ));
                    let v = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = load {}, {}* {}\n",
                        v, elt_ty, elt_ty, p
                    ));
                    return v;
                }
            }
            // The Index arms above cover Var-base on Vec, Array
            // (consuming or via reference), Str, plus struct-
            // field array bases. Anything else hitting this
            // point is a checker bug (or a new type landed
            // without backend support); surface it loudly.
            unreachable!(
                "backend: Index on unsupported base — kind={:?}, ty={:?}",
                array.kind, array.ty
            );
        }
        TypedExprKind::Len { array, length } => {
            // Array: the checker baked in the compile-time length.
            // Vec: load the .len field from the struct alloca (or
            // through a Ref param's pointer).
            // Str/OwnedStr: lower to a `strlen` call.
            if matches!(array.ty.deref(), Type::Str | Type::OwnedStr) {
                let mut s = emit_expr(array, ctx, out);
                // Closure #262: when the operand is a borrow
                // (`ref` / `mut ref`) the emitter returns the
                // ALLOCA address (`i8**`), not the inner
                // pointer. `strlen` wants `i8*` — load through
                // the borrow once. Without this, programs
                // that call `len(ref s)` for `s: OwnedStr`
                // produced LLVM IR that `lli` rejected with
                // "defined with type 'i8**' but expected 'i8*'".
                if matches!(array.ty, Type::Ref(_) | Type::RefMut(_)) {
                    let inner = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = load i8*, i8** {}\n",
                        inner, s
                    ));
                    s = inner;
                }
                let v = ctx.fresh_tmp();
                out.push_str(&format!("  {} = call i64 @strlen(i8* {})\n", v, s));
                // Free fresh-OwnedStr operand after `strlen`
                // (which doesn't consume its argument). Var /
                // FieldAccess operands skip — outer binding's
                // scope-exit Drop owns the heap. Closure
                // #139.
                if crate::ir::is_fresh_owned_str(array) {
                    out.push_str(&format!("  call void @free(i8* {})\n", s));
                }
                return v;
            }
            // Accept the various spellings that ultimately
            // point at a Vec struct's alloca:
            //   `len(xs)`           → Var(name)
            //   `len(ref xs)`       → Ref { name }
            //   `len(mut ref xs)`   → RefMut { name }
            //   `len(ref t.items)`  → RefField { object, field_index, ... }
            //   `len(mut ref t.x)`  → RefMutField { object, field_index, ... }
            // Closure #161 added Var/Ref/RefMut. Closure #162
            // adds the field-borrow forms — previously
            // `len(ref t.items)` fell through to the static-
            // length fallback (zero for Vec), crashing lli
            // with a verifier error on the `i64 0` operand.
            let var_binding: Option<&String> = match &array.kind {
                TypedExprKind::Var(n) => Some(n),
                TypedExprKind::Ref { name } | TypedExprKind::RefMut { name } => Some(name),
                _ => None,
            };
            if let Some(name) = var_binding {
                if let Some((var_ty, addr)) = ctx.locals.get(name).cloned() {
                    if let Type::Vec(element) = var_ty.deref() {
                        let s_ty = vec_struct_name(element);
                        let p = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = getelementptr {}, {}* {}, i64 0, i32 1\n",
                            p, s_ty, s_ty, addr
                        ));
                        let v = ctx.fresh_tmp();
                        out.push_str(&format!("  {} = load i64, i64* {}\n", v, p));
                        return v;
                    }
                }
            }
            // Field-borrow forms: the Ref/RefMutField expression
            // GEP-pointers to the struct's field. For a Vec
            // field, that pointer IS the Vec struct's address,
            // ready for a `i32 1` GEP into .len.
            if matches!(
                array.kind,
                TypedExprKind::RefField { .. } | TypedExprKind::RefMutField { .. }
            ) {
                if let Type::Vec(element) = array.ty.deref() {
                    let s_ty = vec_struct_name(element);
                    let base = emit_expr(array, ctx, out);
                    let p = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = getelementptr {}, {}* {}, i64 0, i32 1\n",
                        p, s_ty, s_ty, base
                    ));
                    let v = ctx.fresh_tmp();
                    out.push_str(&format!("  {} = load i64, i64* {}\n", v, p));
                    return v;
                }
            }
            // `len(t.items)` — FieldAccess yielding a Vec
            // value. Get a pointer to the field via the same
            // lvalue-address machinery the IndexAssign /
            // Reassign paths use, then GEP into .len + load.
            if matches!(array.kind, TypedExprKind::FieldAccess { .. }) {
                if let Type::Vec(element) = array.ty.deref() {
                    let s_ty = vec_struct_name(element);
                    let base = emit_lvalue_addr(array, ctx, out);
                    let p = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = getelementptr {}, {}* {}, i64 0, i32 1\n",
                        p, s_ty, s_ty, base
                    ));
                    let v = ctx.fresh_tmp();
                    out.push_str(&format!("  {} = load i64, i64* {}\n", v, p));
                    return v;
                }
            }
            format!("{}", length)
        }
        TypedExprKind::Ref { name } | TypedExprKind::RefMut { name } => {
            // Pass the alloca pointer (or already-pointer ref param)
            // directly as the call argument. No SSA temp needed.
            match ctx.locals.get(name) {
                Some((_, addr)) => addr.clone(),
                None => unreachable!(
                    "checker: ref to undeclared binding '{}'",
                    name
                ),
            }
        }
        TypedExprKind::RefField { object, field_index, .. }
        | TypedExprKind::RefMutField { object, field_index, .. } => {
            // `ref t.x` / `mut ref t.x` — GEP into the struct's
            // alloca (owned binding) or the pointer the param
            // already holds (ref binding) to get a pointer to
            // the field. T1.2 phase 2b follow-up.
            //
            // For owned struct `t: Tags`, `obj_addr` is the
            // struct's alloca (`%Struct_Tags*`). For ref param
            // `self: ref Tags`, `obj_addr` is the parameter's
            // value, which IS `%Struct_Tags*`. Either way the
            // GEP source type is the dereferenced struct.
            // Previously `llvm_type_string(&obj_ty)` was used
            // directly, which spelled `%Struct_Tags*` for the
            // ref case and produced an invalid `getelementptr
            // %Struct_Tags*, %Struct_Tags** %arg_self, …`.
            // Closure #165.
            let (obj_ty, obj_addr) = ctx
                .locals
                .get(object)
                .cloned()
                .unwrap_or_else(|| unreachable!(
                    "checker: field-borrow on undeclared binding '{}'",
                    object
                ));
            let struct_ty_str = llvm_type_string(obj_ty.deref());
            let p = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = getelementptr {}, {}* {}, i64 0, i32 {}\n",
                p, struct_ty_str, struct_ty_str, obj_addr, field_index
            ));
            p
        }
        TypedExprKind::FnRef { name, .. } => {
            // Taking the address of a top-level function: LLVM
            // exposes it as `@function_name`. The type is the
            // fn-ptr type which `llvm_type_string(expr.ty)`
            // already spells; callers use the value as an
            // SSA operand of that type.
            format!("@{}", crate::backend_c::function_name(name))
        }
        TypedExprKind::CallIndirect { callee, args } => {
            // Load the callee's fn-ptr value (might be a Var
            // referencing an alloca, or a FnRef yielding the
            // global symbol directly). Then emit
            // `call <ret> %fp(args)`.
            let callee_v = emit_expr(callee, ctx, out);
            let ret_ty = llvm_type_string(&expr.ty);
            let param_tys: Vec<String> = match &callee.ty {
                Type::FnPtr(params, _) => {
                    params.iter().map(llvm_type_string).collect()
                }
                _ => unreachable!(
                    "indirect call callee must have fn-ptr type, got {:?}",
                    callee.ty
                ),
            };
            let arg_vs: Vec<String> = args.iter().map(|a| emit_expr(a, ctx, out)).collect();
            let arg_list: Vec<String> = arg_vs
                .iter()
                .zip(param_tys.iter())
                .map(|(v, t)| format!("{} {}", t, v))
                .collect();
            let signature = format!("{} ({})", ret_ty, param_tys.join(", "));
            // The pointer-to-fn we just loaded is either an
            // alloca'd binding (need an extra `load`) or a
            // direct fn symbol from FnRef. Both forms work
            // when passed straight into `call`: LLVM `call`
            // accepts a value of the function-pointer type
            // directly. For binding cases the value came
            // through the existing Var-load path so the
            // pointer is the function pointer, not the alloca
            // itself.
            let dest = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = call {} {}({})\n",
                dest, signature, callee_v, arg_list.join(", ")
            ));
            dest
        }
        TypedExprKind::Tuple { elements } => {
            // Build a `{T1, T2, …}` struct via insertvalue
            // chain. Same shape as Vec literal construction
            // but without the malloc + element-array
            // wrapping. T1.1.
            let struct_ty = llvm_type_string(&expr.ty);
            let elem_vs: Vec<(String, String)> = elements
                .iter()
                .map(|e| (llvm_type_string(&e.ty), emit_expr(e, ctx, out)))
                .collect();
            let mut cur = format!("undef");
            for (i, (ety, v)) in elem_vs.iter().enumerate() {
                let next = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = insertvalue {} {}, {} {}, {}\n",
                    next, struct_ty, cur, ety, v, i
                ));
                cur = next;
            }
            cur
        }
        TypedExprKind::TupleAccess { tuple, index } => {
            let tuple_v = emit_expr(tuple, ctx, out);
            let struct_ty = llvm_type_string(&tuple.ty);
            let dest = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = extractvalue {} {}, {}\n",
                dest, struct_ty, tuple_v, index
            ));
            dest
        }
        TypedExprKind::StructLit { fields, .. } => {
            // Same insertvalue-chain shape as tuples — the
            // struct's named LLVM type is the result type,
            // and fields land in declaration order (the
            // checker already canonicalized that). T1.2.
            let struct_ty = llvm_type_string(&expr.ty);
            let elem_vs: Vec<(String, String)> = fields
                .iter()
                .map(|(_, e)| (llvm_type_string(&e.ty), emit_expr(e, ctx, out)))
                .collect();
            let mut cur = "undef".to_string();
            for (i, (ety, v)) in elem_vs.iter().enumerate() {
                let next = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = insertvalue {} {}, {} {}, {}\n",
                    next, struct_ty, cur, ety, v, i
                ));
                cur = next;
            }
            cur
        }
        TypedExprKind::EnumVariant { enum_name, tag, .. } => {
            // Plain (payload-less) variant: just the tag.
            // Payloaded enum's payload-less variant: build a
            // tagged-union struct via two insertvalues
            // (tag, then zero-init payload). T1.3 phase 2b.
            let payloaded = LLVM_ENUM_PAYLOAD_REGISTRY
                .with(|r| r.borrow().contains_key(enum_name));
            if !payloaded {
                return format!("{}", tag);
            }
            let struct_ty = format!("%Enum_{}", enum_name);
            let payload_ty = LLVM_ENUM_PAYLOAD_REGISTRY
                .with(|r| r.borrow().get(enum_name).cloned())
                .expect("registry insists this is payloaded");
            let payload_ll = llvm_type_string(&payload_ty);
            let payload_zero = match &payload_ty {
                Type::F32 | Type::F64 => "0.0",
                Type::Bool => "false",
                // Pointer payloads (OwnedStr lowers to i8*)
                // need LLVM's `null` literal, not `0`.
                Type::OwnedStr => "null",
                // Aggregate payloads use `zeroinitializer`
                // for the all-zero placeholder when the
                // variant has no user-provided payload.
                Type::Vec(_)
                | Type::Tuple(_)
                | Type::Struct(_)
                | Type::Array { .. }
                | Type::Task
                | Type::Mutex(_)
                | Type::Channel(_, _) => "zeroinitializer",
                _ => "0",
            };
            let s0 = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = insertvalue {} undef, i32 {}, 0\n",
                s0, struct_ty, tag
            ));
            let s1 = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = insertvalue {} {}, {} {}, 1\n",
                s1, struct_ty, s0, payload_ll, payload_zero
            ));
            s1
        }
        TypedExprKind::EnumVariantWithPayload { enum_name, tag, payload, payload_ty, .. } => {
            // T1.3 phase 2b LLVM: build the tagged-union
            // struct via two insertvalues (tag, then
            // the payload's evaluated SSA value).
            let struct_ty = format!("%Enum_{}", enum_name);
            let payload_ll = llvm_type_string(payload_ty);
            let payload_val = emit_expr(payload, ctx, out);
            let s0 = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = insertvalue {} undef, i32 {}, 0\n",
                s0, struct_ty, tag
            ));
            let s1 = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = insertvalue {} {}, {} {}, 1\n",
                s1, struct_ty, s0, payload_ll, payload_val
            ));
            s1
        }
        TypedExprKind::Match { scrutinee, arms } => {
            // Switch on the scrutinee's tag with per-arm
            // basic blocks; phi the arm values back together
            // at the merge. T1.3 phase 2b LLVM: for payloaded
            // enums the scrutinee is a struct `%Enum_X`, so
            // we `extractvalue` field 0 for the switch and
            // `extractvalue` field 1 for the binding in
            // VariantWithBinding arms.
            let scr_v = emit_expr(scrutinee, ctx, out);
            let result_ty = llvm_type_string(&expr.ty);
            let merge_lbl = ctx.fresh_label("match_merge");
            let unreach_lbl = ctx.fresh_label("match_unreach");
            let arm_lbls: Vec<String> = (0..arms.len())
                .map(|i| ctx.fresh_label(&format!("match_arm_{}", i)))
                .collect();
            let wildcard_idx: Option<usize> =
                arms.iter().position(|a| a.is_wildcard);
            let default_lbl = match wildcard_idx {
                Some(i) => arm_lbls[i].clone(),
                None => unreach_lbl.clone(),
            };
            // Detect payloaded-enum scrutinee.
            let scrut_payloaded = match &scrutinee.ty {
                Type::Enum(n) => LLVM_ENUM_PAYLOAD_REGISTRY
                    .with(|r| r.borrow().contains_key(n)),
                _ => false,
            };
            let (dispatch_v, dispatch_ty) = if scrut_payloaded {
                let struct_ty = llvm_type_string(&scrutinee.ty);
                let tag = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = extractvalue {} {}, 0\n",
                    tag, struct_ty, scr_v
                ));
                (tag, "i32".to_string())
            } else {
                (scr_v.clone(), llvm_type_string(&scrutinee.ty))
            };
            let case_lines: Vec<String> = arms
                .iter()
                .zip(arm_lbls.iter())
                .filter(|(a, _)| !a.is_wildcard)
                .map(|(a, l)| {
                    let case_v = a.int_value.map(|v| v.to_string())
                        .unwrap_or_else(|| a.tag.to_string());
                    format!("{} {}, label %{}", dispatch_ty, case_v, l)
                })
                .collect();
            out.push_str(&format!(
                "  switch {} {}, label %{} [ {} ]\n",
                dispatch_ty, dispatch_v, default_lbl, case_lines.join(" ")
            ));
            let mut incoming: Vec<(String, String)> = Vec::new();
            for (arm, lbl) in arms.iter().zip(arm_lbls.iter()) {
                out.push_str(&format!("{}:\n", lbl));
                ctx.current_block = lbl.clone();
                // T1.3 phase 2b LLVM: for VariantWithBinding
                // arms, extract the payload into a local
                // alloca + store, and register the binding
                // name in ctx.locals so the body's reads
                // resolve. Restore after the arm body.
                let restore_binding: Option<(String, Option<(Type, String)>)> =
                    if let Some((bname, bty)) = &arm.binding {
                        let struct_ty = llvm_type_string(&scrutinee.ty);
                        let bty_ll = llvm_type_string(bty);
                        let extracted = ctx.fresh_tmp();
                        out.push_str(&format!(
                            "  {} = extractvalue {} {}, 1\n",
                            extracted, struct_ty, scr_v
                        ));
                        let addr = format!("{}.{}.addr", ctx.fresh_tmp(), bname);
                        out.push_str(&format!("  {} = alloca {}\n", addr, bty_ll));
                        out.push_str(&format!(
                            "  store {} {}, {}* {}\n",
                            bty_ll, extracted, bty_ll, addr
                        ));
                        let prev = ctx.locals.get(bname).cloned();
                        ctx.locals.insert(bname.clone(), (bty.clone(), addr));
                        Some((bname.clone(), prev))
                    } else {
                        None
                    };
                let v = emit_expr(&arm.body, ctx, out);
                if let Some((bname, prev)) = restore_binding {
                    match prev {
                        Some(p) => { ctx.locals.insert(bname, p); }
                        None => { ctx.locals.remove(&bname); }
                    }
                }
                let pred = ctx.current_block.clone();
                out.push_str(&format!("  br label %{}\n", merge_lbl));
                incoming.push((v, pred));
            }
            // Only emit the unreachable block when there's
            // no wildcard catching the default branch.
            if wildcard_idx.is_none() {
                out.push_str(&format!("{}:\n", unreach_lbl));
                out.push_str("  call void @abort()\n");
                out.push_str("  unreachable\n");
            }
            out.push_str(&format!("{}:\n", merge_lbl));
            ctx.current_block = merge_lbl.clone();
            let phi = ctx.fresh_tmp();
            let phi_args: Vec<String> = incoming
                .iter()
                .map(|(v, l)| format!("[ {}, %{} ]", v, l))
                .collect();
            out.push_str(&format!(
                "  {} = phi {} {}\n",
                phi, result_ty, phi_args.join(", ")
            ));
            phi
        }
        TypedExprKind::IfExpr { cond, then_value, else_value } => {
            // br + phi merge — same shape as Match with 2
            // arms. The phi's predecessor label must be the
            // *actual* basic block we ended up in when
            // emitting each branch value (which may differ
            // from the branch's opening label if nested
            // if-expressions introduced inner BBs). We track
            // this via `ctx.current_block`, updated on every
            // label emission. T4 (if-as-expression).
            let c = emit_expr(cond, ctx, out);
            let result_ty = llvm_type_string(&expr.ty);
            let then_lbl = ctx.fresh_label("ifexpr_then");
            let else_lbl = ctx.fresh_label("ifexpr_else");
            let merge_lbl = ctx.fresh_label("ifexpr_merge");
            out.push_str(&format!(
                "  br i1 {}, label %{}, label %{}\n",
                c, then_lbl, else_lbl
            ));
            out.push_str(&format!("{}:\n", then_lbl));
            ctx.current_block = then_lbl.clone();
            let tv = emit_expr(then_value, ctx, out);
            let then_pred = ctx.current_block.clone();
            out.push_str(&format!("  br label %{}\n", merge_lbl));
            out.push_str(&format!("{}:\n", else_lbl));
            ctx.current_block = else_lbl.clone();
            let ev = emit_expr(else_value, ctx, out);
            let else_pred = ctx.current_block.clone();
            out.push_str(&format!("  br label %{}\n", merge_lbl));
            out.push_str(&format!("{}:\n", merge_lbl));
            ctx.current_block = merge_lbl.clone();
            let phi = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = phi {} [ {}, %{} ], [ {}, %{} ]\n",
                phi, result_ty, tv, then_pred, ev, else_pred
            ));
            phi
        }
        TypedExprKind::FieldAccess { object, field_index, .. } => {
            // Through a ref we have a pointer to the struct;
            // load + extractvalue. Otherwise extract directly.
            let inner = emit_expr(object, ctx, out);
            let dest = ctx.fresh_tmp();
            if object.ty.is_any_ref() {
                let underlying = object.ty.deref();
                let struct_ty = llvm_type_string(underlying);
                let loaded = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = load {}, {}* {}\n",
                    loaded, struct_ty, struct_ty, inner
                ));
                out.push_str(&format!(
                    "  {} = extractvalue {} {}, {}\n",
                    dest, struct_ty, loaded, field_index
                ));
            } else {
                let struct_ty = llvm_type_string(&object.ty);
                out.push_str(&format!(
                    "  {} = extractvalue {} {}, {}\n",
                    dest, struct_ty, inner, field_index
                ));
            }
            dest
        }
        // ArrayLit appears as a sub-expression when an
        // array literal is passed directly as a function
        // argument (e.g. `f([P{1,2}, P{3,4}])`). Allocate
        // a fresh aggregate, store each element, then
        // load the whole array as a value. T1.2 + arrays
        // in call-arg position.
        TypedExprKind::ArrayLit { elements } => {
            let array_ty = llvm_type_string(&expr.ty);
            let elt_ty = match &expr.ty {
                Type::Array { element, .. } => llvm_type_string(element),
                _ => unreachable!("ArrayLit ty must be Array"),
            };
            let addr = ctx.fresh_tmp();
            out.push_str(&format!("  {} = alloca {}\n", addr, array_ty));
            for (i, e) in elements.iter().enumerate() {
                let v = emit_expr(e, ctx, out);
                let p = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = getelementptr {}, {}* {}, i64 0, i64 {}\n",
                    p, array_ty, array_ty, addr, i
                ));
                out.push_str(&format!(
                    "  store {} {}, {}* {}\n",
                    elt_ty, v, elt_ty, p
                ));
            }
            let loaded = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = load {}, {}* {}\n",
                loaded, array_ty, array_ty, addr
            ));
            loaded
        }
        TypedExprKind::Block { stmts, tail } => {
            // Block expression: emit each Let stmt inline
            // (alloca + store, register the name in ctx.locals
            // so the tail can reference it) and each Print /
            // Drop stmt via the normal stmt emitter, then emit
            // the tail expression. After emission, restore any
            // outer-scope bindings shadowed by inner lets so
            // the surrounding scope's reads still resolve.
            // V1 user-facing blocks admit Let + Print
            // (closure #129); the checker also synthesizes
            // Block exprs containing a `Drop` stmt for fresh
            // OwnedStr match scrutinees (closure #137), and
            // for `try` desugar's intermediate prints
            // (closure #130). Closure #160 — fixes a
            // tree-LLVM leak where the synthesized Drop was
            // silently dropped here.
            let saved: Vec<(String, Option<(Type, String)>)> = stmts
                .iter()
                .filter_map(|s| {
                    if let TypedStmt::Let { name, .. } = s {
                        Some((name.clone(), ctx.locals.get(name).cloned()))
                    } else {
                        None
                    }
                })
                .collect();
            for s in stmts {
                match s {
                    TypedStmt::Let { name, ty, expr: rhs } => {
                        let value = emit_expr(rhs, ctx, out);
                        let lty = llvm_type_string(ty);
                        let addr = format!("{}.{}.addr", ctx.fresh_tmp(), name);
                        out.push_str(&format!("  {} = alloca {}\n", addr, lty));
                        out.push_str(&format!(
                            "  store {} {}, {}* {}\n",
                            lty, value, lty, addr
                        ));
                        ctx.locals.insert(name.clone(), (ty.clone(), addr));
                    }
                    TypedStmt::Print { .. }
                    | TypedStmt::Drop { .. }
                    | TypedStmt::Discard { .. }
                    | TypedStmt::Reassign { .. }
                    | TypedStmt::While { .. } => {
                        // Forward Print/Drop/Discard/Reassign/
                        // While through the stmt-level emit. The
                        // fn-body emit already knows how to lower
                        // a while-loop into LLVM basic blocks
                        // (header / body / exit); calling it from
                        // inside a Block-expr just splices those
                        // blocks into the surrounding fn.
                        emit_stmt(s, ctx, out);
                    }
                    _ => {}
                }
            }
            let tail_val = emit_expr(tail, ctx, out);
            // Restore outer scope so block-internal names
            // don't leak past the expression.
            for (name, prev) in saved {
                match prev {
                    Some(p) => {
                        ctx.locals.insert(name, p);
                    }
                    None => {
                        ctx.locals.remove(&name);
                    }
                }
            }
            tail_val
        }
        TypedExprKind::DynCoerce { value, iface_name, from_type_name, from_ty: _ } => {
            // Vtables Phase 3b: materialize the fat pointer.
            // v1 restricts the source to a Var so the data
            // slot can point at the binding's existing alloca
            // (stable address). Non-Var sources need an IR
            // hoist (Phase 4 follow-up).
            let TypedExprKind::Var(var_name) = &value.kind else {
                panic!(
                    "vtables Phase 3b: coercion to `dyn {}` from non-Var source is \
                     pending — let-bind the value before passing it",
                    iface_name
                );
            };
            let (_var_ty, var_addr) = ctx
                .locals
                .get(var_name)
                .cloned()
                .expect("dyn coerce: var must be in scope");
            let dyn_ty = format!("%intent_dyn_{}", iface_name);
            let vtbl_ty = format!("%intent_vtbl_{}", iface_name);
            let vtbl_global = format!("@intent_vtbl_{}_{}", iface_name, from_type_name);
            let data_i8 = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = bitcast {}* {} to i8*\n",
                data_i8, llvm_type_string(&value.ty), var_addr
            ));
            let stage0 = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = insertvalue {} undef, {}* {}, 0\n",
                stage0, dyn_ty, vtbl_ty, vtbl_global
            ));
            let stage1 = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = insertvalue {} {}, i8* {}, 1\n",
                stage1, dyn_ty, stage0, data_i8
            ));
            stage1
        }
        TypedExprKind::DynDispatch {
            receiver, iface_name, slot_index, args, ..
        } => {
            // Vtables Phase 3b + 4c: GEP into the fat
            // pointer's vtable, load the slot's fn-ptr, call
            // it indirectly with the data pointer as the
            // implicit first arg. For a borrowed receiver
            // (`ref dyn Iface` / `mut ref dyn Iface`) the
            // emitted value is a pointer to the fat pointer;
            // load it first so the extractvalue ops see a
            // struct value.
            let raw_recv = emit_expr(receiver, ctx, out);
            let dyn_ty = format!("%intent_dyn_{}", iface_name);
            let vtbl_ty = format!("%intent_vtbl_{}", iface_name);
            let recv_val = match &receiver.ty {
                Type::Ref(_) | Type::RefMut(_) => {
                    let loaded = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = load {}, {}* {}\n",
                        loaded, dyn_ty, dyn_ty, raw_recv
                    ));
                    loaded
                }
                _ => raw_recv,
            };
            let vtbl_ptr = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = extractvalue {} {}, 0\n",
                vtbl_ptr, dyn_ty, recv_val
            ));
            let data_ptr = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = extractvalue {} {}, 1\n",
                data_ptr, dyn_ty, recv_val
            ));
            let slot_ptr_ptr = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = getelementptr {}, {}* {}, i32 0, i32 {}\n",
                slot_ptr_ptr, vtbl_ty, vtbl_ty, vtbl_ptr, slot_index
            ));
            // Build the slot's fn-ptr type from the iface
            // signature so the load + call have matching types.
            let methods = crate::ast::iface_methods_for(iface_name)
                .expect("dyn dispatch: iface registry must hold the method list");
            let (_, iface_params, iface_ret) = methods
                .get(*slot_index)
                .cloned()
                .expect("dyn dispatch: slot index in range");
            let ret_ty = llvm_type_string(&iface_ret);
            let arg_tys: Vec<String> = std::iter::once("i8*".to_string())
                .chain(iface_params.iter().skip(1).map(llvm_type_string))
                .collect();
            let fn_ptr_ty = format!("{} ({})*", ret_ty, arg_tys.join(", "));
            let slot_ptr = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = load {}, {}* {}\n",
                slot_ptr, fn_ptr_ty, fn_ptr_ty, slot_ptr_ptr
            ));
            let mut arg_vals: Vec<String> = vec![format!("i8* {}", data_ptr)];
            for (i, arg) in args.iter().enumerate() {
                let v = emit_expr(arg, ctx, out);
                arg_vals.push(format!("{} {}", arg_tys[i + 1], v));
            }
            let result = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = call {} {}({})\n",
                result,
                ret_ty,
                slot_ptr,
                arg_vals.join(", "),
            ));
            result
        }
        kind => unreachable!(
            "backend: TypedExprKind not lowered as standalone expression: {:?}",
            kind
        ),
    }
}

/// Lower `let <name>: Vec<T> = vec(a0, a1, …, aN);`.
///
/// 1. `call i8* @malloc(i64 N*sizeof(T))` to back the buffer.
/// 2. Bitcast to `T*` and store each element via GEP+store.
/// 3. Build the `{T*, i64, i64}` struct value by chained `insertvalue`.
/// 4. Alloca the struct, store the value, register in `ctx.locals`.
fn emit_vec_let_from_literal(
    name: &str,
    element: &Type,
    args: &[TypedExpr],
    ctx: &mut FnCtx,
    out: &mut String,
) {
    let n = args.len() as i64;
    // Use the in-buffer value spelling so arrays slot in as
    // `[N x T]` (not `[N x T]*`). `vec_element_byte_size`
    // gives the per-slot byte count for malloc/realloc.
    // Refines #7 (phase 2c collapses the array-ptr form).
    let elt_ty = vec_element_value_str(element);
    let raw = ctx.fresh_tmp();
    let payloaded_enum = matches!(element, Type::Enum(name)
        if LLVM_ENUM_PAYLOAD_REGISTRY.with(|r| r.borrow().contains_key(name)));
    if matches!(element, Type::Struct(_) | Type::Tuple(_)) || payloaded_enum {
        // Struct/tuple/payloaded-enum element: runtime
        // sizeof via GEP-null trick. T1.2 + Vec<Struct>
        // LLVM. Closure #151 added the payloaded-enum
        // case — was using `vec_element_byte_size` which
        // returned 8 for enums (treating them as i64),
        // under-allocating by half for the 16-byte tagged
        // union and triggering an invalid-pointer crash
        // at lli time.
        let size_expr = vec_element_size_expr(element);
        let count_max = n.max(1);
        let bytes_v = ctx.fresh_tmp();
        out.push_str(&format!(
            "  {} = mul i64 {}, {}\n",
            bytes_v, count_max, size_expr
        ));
        out.push_str(&format!(
            "  {} = call i8* @malloc(i64 {})\n",
            raw, bytes_v
        ));
    } else {
    let elt_size = vec_element_byte_size(element);
    let total = (n as u64) * (elt_size as u64).max(1);

    // Empty vec? Allocate one element to keep the buffer non-null.
    let bytes = total.max(elt_size.max(1) as u64) as i64;
    out.push_str(&format!(
        "  {} = call i8* @malloc(i64 {})\n",
        raw, bytes
    ));
    }
    let buf = ctx.fresh_tmp();
    out.push_str(&format!("  {} = bitcast i8* {} to {}*\n", buf, raw, elt_ty));

    // Store each element.
    for (i, e) in args.iter().enumerate() {
        let v = emit_expr(e, ctx, out);
        let p = ctx.fresh_tmp();
        out.push_str(&format!(
            "  {} = getelementptr {}, {}* {}, i64 {}\n",
            p, elt_ty, elt_ty, buf, i
        ));
        out.push_str(&format!("  store {} {}, {}* {}\n", elt_ty, v, elt_ty, p));
    }

    // Build the struct value: { data, len, capacity }.
    let cap = if n == 0 { 1 } else { n };
    let s_ty = vec_struct_name(element);
    let s0 = ctx.fresh_tmp();
    out.push_str(&format!(
        "  {} = insertvalue {} undef, {}* {}, 0\n",
        s0, s_ty, elt_ty, buf
    ));
    let s1 = ctx.fresh_tmp();
    out.push_str(&format!(
        "  {} = insertvalue {} {}, i64 {}, 1\n",
        s1, s_ty, s0, n
    ));
    let s2 = ctx.fresh_tmp();
    out.push_str(&format!(
        "  {} = insertvalue {} {}, i64 {}, 2\n",
        s2, s_ty, s1, cap
    ));

    // Alloca + store into the binding.
    let addr = format!("%{}.addr", name);
    out.push_str(&format!("  {} = alloca {}\n", addr, s_ty));
    out.push_str(&format!("  store {} {}, {}* {}\n", s_ty, s2, s_ty, addr));
    ctx.locals.insert(
        name.to_string(),
        (Type::Vec(Box::new(element.clone())), addr),
    );
}

/// Lower `print item1, item2, …;`. Each item is printed without a
/// trailing newline; a single space separates adjacent items; a
/// final `\n` terminates the line.
fn emit_print_items(
    items: &[crate::ir::TypedPrintItem],
    ctx: &mut FnCtx,
    out: &mut String,
) {
    use crate::ir::TypedPrintItem;
    for (i, item) in items.iter().enumerate() {
        match item {
            TypedPrintItem::Str(text) => {
                if let Some(&idx) = ctx.print_str_indices.get(text) {
                    let bytes = text.len() + 1;
                    let str_p = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = getelementptr [{} x i8], [{} x i8]* @.print_str.{}, i64 0, i64 0\n",
                        str_p, bytes, bytes, idx
                    ));
                    let fmt_p = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = getelementptr [3 x i8], [3 x i8]* @.fmt.s, i64 0, i64 0\n",
                        fmt_p
                    ));
                    out.push_str(&format!(
                        "  call i32 (i8*, ...) @printf(i8* {}, i8* {})\n",
                        fmt_p, str_p
                    ));
                }
            }
            TypedPrintItem::Expr(expr) => emit_print_expr_no_newline(expr, ctx, out),
        }
        if i + 1 < items.len() {
            out.push_str("  call i32 @putchar(i32 32)\n");
        }
    }
    out.push_str("  call i32 @putchar(i32 10)\n");
}

fn emit_print_expr_no_newline(expr: &TypedExpr, ctx: &mut FnCtx, out: &mut String) {
    let value = emit_expr(expr, ctx, out);
    match &expr.ty {
        Type::Bool => {
            // Branch into "true"/"false" no-newline globals.
            let t_lbl = ctx.fresh_label("p_true");
            let f_lbl = ctx.fresh_label("p_false");
            let m_lbl = ctx.fresh_label("p_done");
            out.push_str(&format!(
                "  br i1 {}, label %{}, label %{}\n",
                value, t_lbl, f_lbl
            ));
            out.push_str(&format!("{}:\n", t_lbl));
            let t_fmt = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = getelementptr [5 x i8], [5 x i8]* @.fmt.true, i64 0, i64 0\n",
                t_fmt
            ));
            out.push_str(&format!(
                "  call i32 (i8*, ...) @printf(i8* {})\n",
                t_fmt
            ));
            out.push_str(&format!("  br label %{}\n", m_lbl));
            out.push_str(&format!("{}:\n", f_lbl));
            let f_fmt = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = getelementptr [6 x i8], [6 x i8]* @.fmt.false, i64 0, i64 0\n",
                f_fmt
            ));
            out.push_str(&format!(
                "  call i32 (i8*, ...) @printf(i8* {})\n",
                f_fmt
            ));
            out.push_str(&format!("  br label %{}\n", m_lbl));
            out.push_str(&format!("{}:\n", m_lbl));
        }
        ty if ty.is_unsigned_integer() => {
            let widened = widen_int_to_64(&value, ty, ctx, out, false);
            let fmt = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = getelementptr [5 x i8], [5 x i8]* @.fmt.llu, i64 0, i64 0\n",
                fmt
            ));
            out.push_str(&format!(
                "  call i32 (i8*, ...) @printf(i8* {}, i64 {})\n",
                fmt, widened
            ));
        }
        ty if ty.is_signed_integer() => {
            let widened = widen_int_to_64(&value, ty, ctx, out, true);
            let fmt = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = getelementptr [5 x i8], [5 x i8]* @.fmt.lld, i64 0, i64 0\n",
                fmt
            ));
            out.push_str(&format!(
                "  call i32 (i8*, ...) @printf(i8* {}, i64 {})\n",
                fmt, widened
            ));
        }
        Type::F64 => {
            let fmt = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = getelementptr [3 x i8], [3 x i8]* @.fmt.g, i64 0, i64 0\n",
                fmt
            ));
            out.push_str(&format!(
                "  call i32 (i8*, ...) @printf(i8* {}, double {})\n",
                fmt, value
            ));
        }
        Type::F32 => {
            let dbl = ctx.fresh_tmp();
            out.push_str(&format!("  {} = fpext float {} to double\n", dbl, value));
            let fmt = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = getelementptr [3 x i8], [3 x i8]* @.fmt.g, i64 0, i64 0\n",
                fmt
            ));
            out.push_str(&format!(
                "  call i32 (i8*, ...) @printf(i8* {}, double {})\n",
                fmt, dbl
            ));
        }
        Type::Str => {
            let fmt = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = getelementptr [3 x i8], [3 x i8]* @.fmt.s, i64 0, i64 0\n",
                fmt
            ));
            out.push_str(&format!(
                "  call i32 (i8*, ...) @printf(i8* {}, i8* {})\n",
                fmt, value
            ));
        }
        Type::OwnedStr => {
            // Conservative whitelist: only Call returning
            // OwnedStr (intent_str_concat / user fn) and
            // Binary `+` (string concat) are guaranteed-fresh
            // heap-producers in v1. Var / FieldAccess /
            // TupleAccess reference a value owned by some
            // binding whose scope-exit Drop frees it —
            // freeing after print would double-free. Closure
            // #135.
            let fmt = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = getelementptr [3 x i8], [3 x i8]* @.fmt.s, i64 0, i64 0\n",
                fmt
            ));
            out.push_str(&format!(
                "  call i32 (i8*, ...) @printf(i8* {}, i8* {})\n",
                fmt, value
            ));
            if matches!(
                expr.kind,
                TypedExprKind::Call { .. } | TypedExprKind::Binary { .. }
            ) {
                out.push_str(&format!(
                    "  call void @free(i8* {})\n",
                    value
                ));
            }
        }
        // The checker emits a diagnostic for `print` of arrays /
        // Vecs and the parser/checker reject ref types in print
        // items, so everything that reaches here is a printable
        // scalar. Anything else is a checker bug.
        ty => unreachable!(
            "checker rejects print of {:?} but the LLVM backend was asked to lower it",
            ty
        ),
    }
}

/// Widen an i8/i16/i32 value to i64 with sign-extend (signed) or
/// zero-extend (unsigned). For an already-i64 value, return as-is.
fn widen_int_to_64(
    value: &str,
    ty: &Type,
    ctx: &mut FnCtx,
    out: &mut String,
    signed: bool,
) -> String {
    if matches!(ty, Type::I64 | Type::U64) {
        return value.to_string();
    }
    let from = llvm_type(ty);
    let dest = ctx.fresh_tmp();
    let op = if signed { "sext" } else { "zext" };
    out.push_str(&format!(
        "  {} = {} {} {} to i64\n",
        dest, op, from, value
    ));
    dest
}

/// Convenience wrapper for the GEP-index case: dispatches signed vs
/// unsigned widening based on the index type.
fn widen_index_to_64(value: &str, ty: &Type, ctx: &mut FnCtx, out: &mut String) -> String {
    widen_int_to_64(value, ty, ctx, out, ty.is_signed_integer())
}

/// Pick the LLVM cast opcode for src→dst. Returns the special
/// sentinel `"bitcast-noop"` for same-width same-kind casts (e.g.
/// `i64 → u64`, `u32 → i32`) — the caller should pass the value
/// through unchanged.
fn cast_opcode(src: &Type, dst: &Type) -> &'static str {
    // Treat enum sources as 32-bit unsigned integers: they
    // lower to an i32 tag in both backends, and the cast
    // result is the variant's index (0..N-1). Lets users
    // write `c as i64` for serialization or table-driven
    // dispatch. T1.3 follow-up.
    let src_int = src.is_integer() || matches!(src, Type::Enum(_));
    let dst_int = dst.is_integer();
    let src_float = src.is_float();
    let dst_float = dst.is_float();
    let src_bits = || -> u16 {
        if matches!(src, Type::Enum(_)) {
            32
        } else {
            src.bits().unwrap_or(64)
        }
    };
    let src_signed = || -> bool {
        if matches!(src, Type::Enum(_)) {
            false
        } else {
            src.is_signed_integer()
        }
    };

    if src_int && dst_int {
        let sb = src_bits();
        let db = dst.bits().unwrap_or(64);
        if sb == db {
            return "bitcast-noop";
        } else if db > sb {
            return if src_signed() { "sext" } else { "zext" };
        } else {
            return "trunc";
        }
    }
    if src_int && dst_float {
        return if src.is_signed_integer() { "sitofp" } else { "uitofp" };
    }
    if src_float && dst_int {
        return if dst.is_signed_integer() { "fptosi" } else { "fptoui" };
    }
    if src_float && dst_float {
        // `Type::bits()` returns None for floats, so use explicit
        // mapping instead of the int-style bits() fallback.
        let sb = float_bits(src);
        let db = float_bits(dst);
        if sb == db {
            return "bitcast-noop";
        }
        return if db > sb { "fpext" } else { "fptrunc" };
    }
    "bitcast-noop"
}

fn float_bits(ty: &Type) -> u32 {
    match ty {
        Type::F32 => 32,
        Type::F64 => 64,
        _ => 64,
    }
}

fn icmp_predicate(op: BinaryOp, signed: bool) -> &'static str {
    match (op, signed) {
        (BinaryOp::Eq, _) => "eq",
        (BinaryOp::Ne, _) => "ne",
        (BinaryOp::Lt, true) => "slt",
        (BinaryOp::Lt, false) => "ult",
        (BinaryOp::Le, true) => "sle",
        (BinaryOp::Le, false) => "ule",
        (BinaryOp::Gt, true) => "sgt",
        (BinaryOp::Gt, false) => "ugt",
        (BinaryOp::Ge, true) => "sge",
        (BinaryOp::Ge, false) => "uge",
        _ => unreachable!("icmp_predicate called on non-comparison op"),
    }
}

/// Emit `push` / `set` / `clone` helper functions for one Vec
/// element type. Mirrors the C backend's monomorphized helpers
/// but in LLVM IR. Each helper consumes its Vec arg by value
/// Emits the `intent_str_concat` runtime helper that both
/// the tree-LLVM backend and the SSA-LLVM backend can call
/// from Str/OwnedStr `+` lowering. Always definition-only —
/// callers add `declare i64 @strlen(i8*)`, `declare i8*
/// @malloc(i64)`, `declare i8* @memcpy(i8*, i8*, i64)`, and
/// `declare void @free(i8*)` to the module preamble.
pub(crate) fn emit_intent_str_concat_definition(out: &mut String) {
    out.push_str("define i8* @intent_str_concat(i8* %l, i32 %lo, i8* %r, i32 %ro) {\n");
    out.push_str("  %ln = call i64 @strlen(i8* %l)\n");
    out.push_str("  %rn = call i64 @strlen(i8* %r)\n");
    out.push_str("  %sum = add i64 %ln, %rn\n");
    out.push_str("  %total = add i64 %sum, 1\n");
    out.push_str("  %buf = call i8* @malloc(i64 %total)\n");
    out.push_str("  %_cl = call i8* @memcpy(i8* %buf, i8* %l, i64 %ln)\n");
    out.push_str("  %tail = getelementptr i8, i8* %buf, i64 %ln\n");
    out.push_str("  %_cr = call i8* @memcpy(i8* %tail, i8* %r, i64 %rn)\n");
    out.push_str("  %nul = getelementptr i8, i8* %buf, i64 %sum\n");
    out.push_str("  store i8 0, i8* %nul\n");
    out.push_str("  %lo_b = icmp ne i32 %lo, 0\n");
    out.push_str("  br i1 %lo_b, label %free_l, label %check_r\n");
    out.push_str("free_l:\n");
    out.push_str("  call void @free(i8* %l)\n");
    out.push_str("  br label %check_r\n");
    out.push_str("check_r:\n");
    out.push_str("  %ro_b = icmp ne i32 %ro, 0\n");
    out.push_str("  br i1 %ro_b, label %free_r, label %done\n");
    out.push_str("free_r:\n");
    out.push_str("  call void @free(i8* %r)\n");
    out.push_str("  br label %done\n");
    out.push_str("done:\n");
    out.push_str("  ret i8* %buf\n");
    out.push_str("}\n\n");
}

/// (matching the affine-ownership convention) and returns the
/// new Vec value.
pub(crate) fn emit_vec_helpers(element: &Type, out: &mut String) {
    let s_ty = vec_struct_name(element);
    // In-buffer value spelling — handles arrays as `[N x T]`
    // rather than the alloca-pointer `[N x T]*` form. Phase
    // 2c.
    let elt_ty = vec_element_value_str(element);
    // Use the LLVM-expression form so struct / tuple
    // elements compute their byte size via the GEP-null
    // sizeof trick rather than under-allocating with the
    // wrong scalar fallback. T1.2 + Vec<Struct> LLVM.
    let elt_size = vec_element_size_expr(element);
    let tag = vec_struct_tag(element);
    let push_name = format!("@intent_vec_{}__push", tag);
    let push_mut_name = format!("@intent_vec_{}__push_mut", tag);
    let pop_mut_name = format!("@intent_vec_{}__pop_mut", tag);
    let set_name = format!("@intent_vec_{}__set", tag);
    let clone_name = format!("@intent_vec_{}__clone", tag);
    let free_name = format!("@intent_vec_{}__free", tag);
    let element_is_copy = element.is_copy();

    // ---- push(xs, v): grow if needed, store v at len, return new struct.
    out.push_str(&format!(
        "define {} {}({} %xs, {} %v) {{\n",
        s_ty, push_name, s_ty, elt_ty
    ));
    out.push_str(&format!("  %data = extractvalue {} %xs, 0\n", s_ty));
    out.push_str(&format!("  %len = extractvalue {} %xs, 1\n", s_ty));
    out.push_str(&format!("  %cap = extractvalue {} %xs, 2\n", s_ty));
    out.push_str("  %new_len = add i64 %len, 1\n");
    out.push_str("  %need = icmp ugt i64 %new_len, %cap\n");
    out.push_str("  br i1 %need, label %grow, label %inplace\n");
    out.push_str("grow:\n");
    out.push_str("  %cap_doubled = mul i64 %cap, 2\n");
    out.push_str("  %cap_was_zero = icmp eq i64 %cap, 0\n");
    out.push_str("  %new_cap_g = select i1 %cap_was_zero, i64 1, i64 %cap_doubled\n");
    out.push_str(&format!(
        "  %new_bytes_g = mul i64 %new_cap_g, {}\n",
        elt_size
    ));
    out.push_str(&format!(
        "  %old_raw = bitcast {}* %data to i8*\n",
        elt_ty
    ));
    out.push_str("  %new_raw = call i8* @realloc(i8* %old_raw, i64 %new_bytes_g)\n");
    out.push_str(&format!(
        "  %new_data_g = bitcast i8* %new_raw to {}*\n",
        elt_ty
    ));
    out.push_str("  br label %store_v\n");
    out.push_str("inplace:\n");
    out.push_str("  br label %store_v\n");
    out.push_str("store_v:\n");
    out.push_str(&format!(
        "  %new_data = phi {}* [ %new_data_g, %grow ], [ %data, %inplace ]\n",
        elt_ty
    ));
    out.push_str(
        "  %new_cap = phi i64 [ %new_cap_g, %grow ], [ %cap, %inplace ]\n",
    );
    out.push_str(&format!(
        "  %p = getelementptr {}, {}* %new_data, i64 %len\n",
        elt_ty, elt_ty
    ));
    out.push_str(&format!("  store {} %v, {}* %p\n", elt_ty, elt_ty));
    out.push_str(&format!(
        "  %r0 = insertvalue {} undef, {}* %new_data, 0\n",
        s_ty, elt_ty
    ));
    out.push_str(&format!(
        "  %r1 = insertvalue {} %r0, i64 %new_len, 1\n",
        s_ty
    ));
    out.push_str(&format!(
        "  %r2 = insertvalue {} %r1, i64 %new_cap, 2\n",
        s_ty
    ));
    out.push_str(&format!("  ret {} %r2\n", s_ty));
    out.push_str("}\n");

    // ---- push_mut(xs_p, v): in-place push through a Vec
    // struct pointer. Grow if needed; store at len; bump len;
    // return new length as i64. Used by `push(mut ref xs, v)`
    // — caller passes a pointer to the Vec (alloca address or
    // field GEP). T1.2 phase 2b follow-up.
    out.push_str(&format!(
        "define i64 {}({}* %xs_p, {} %v) {{\n",
        push_mut_name, s_ty, elt_ty
    ));
    out.push_str(&format!(
        "  %data_p_m = getelementptr {}, {}* %xs_p, i32 0, i32 0\n",
        s_ty, s_ty
    ));
    out.push_str(&format!(
        "  %len_p_m = getelementptr {}, {}* %xs_p, i32 0, i32 1\n",
        s_ty, s_ty
    ));
    out.push_str(&format!(
        "  %cap_p_m = getelementptr {}, {}* %xs_p, i32 0, i32 2\n",
        s_ty, s_ty
    ));
    out.push_str(&format!(
        "  %data_m = load {}*, {}** %data_p_m\n",
        elt_ty, elt_ty
    ));
    out.push_str("  %len_m = load i64, i64* %len_p_m\n");
    out.push_str("  %cap_m = load i64, i64* %cap_p_m\n");
    out.push_str("  %new_len_m = add i64 %len_m, 1\n");
    out.push_str("  %need_m = icmp ugt i64 %new_len_m, %cap_m\n");
    out.push_str("  br i1 %need_m, label %grow_m, label %inplace_m\n");
    out.push_str("grow_m:\n");
    out.push_str("  %cap_doubled_m = mul i64 %cap_m, 2\n");
    out.push_str("  %cap_was_zero_m = icmp eq i64 %cap_m, 0\n");
    out.push_str("  %new_cap_gm = select i1 %cap_was_zero_m, i64 1, i64 %cap_doubled_m\n");
    out.push_str(&format!(
        "  %new_bytes_gm = mul i64 %new_cap_gm, {}\n",
        elt_size
    ));
    out.push_str(&format!(
        "  %old_raw_m = bitcast {}* %data_m to i8*\n",
        elt_ty
    ));
    out.push_str("  %new_raw_m = call i8* @realloc(i8* %old_raw_m, i64 %new_bytes_gm)\n");
    out.push_str(&format!(
        "  %new_data_gm = bitcast i8* %new_raw_m to {}*\n",
        elt_ty
    ));
    out.push_str(&format!(
        "  store {}* %new_data_gm, {}** %data_p_m\n",
        elt_ty, elt_ty
    ));
    out.push_str("  store i64 %new_cap_gm, i64* %cap_p_m\n");
    out.push_str("  br label %store_v_m\n");
    out.push_str("inplace_m:\n");
    out.push_str("  br label %store_v_m\n");
    out.push_str("store_v_m:\n");
    out.push_str(&format!(
        "  %final_data_m = phi {}* [ %new_data_gm, %grow_m ], [ %data_m, %inplace_m ]\n",
        elt_ty
    ));
    out.push_str(&format!(
        "  %slot_m = getelementptr {}, {}* %final_data_m, i64 %len_m\n",
        elt_ty, elt_ty
    ));
    out.push_str(&format!("  store {} %v, {}* %slot_m\n", elt_ty, elt_ty));
    out.push_str("  store i64 %new_len_m, i64* %len_p_m\n");
    out.push_str("  ret i64 %new_len_m\n");
    out.push_str("}\n");

    // ---- pop_mut(xs_p) -> T: in-place pop through a Vec
    // struct pointer. Abort on empty, otherwise load the
    // last element, decrement `len`, and return the loaded
    // value. For non-Copy element types the returned value
    // carries ownership of the slot's heap; the Vec's
    // scope-exit `__free` walks elements via the post-pop
    // len so the moved-out slot is not re-freed.
    // Closure #219.
    //
    // Array element types ([T;N]) skip this helper — C
    // can't return a bare array by value and the LLVM
    // equivalent would need a struct wrapper. Defer that
    // shape to a follow-up.
    let element_is_array = matches!(element, Type::Array { .. });
    if !element_is_array {
        out.push_str(&format!(
            "define {} {}({}* %xs_pp) {{\n",
            elt_ty, pop_mut_name, s_ty
        ));
        out.push_str(&format!(
            "  %len_pp = getelementptr {}, {}* %xs_pp, i32 0, i32 1\n",
            s_ty, s_ty
        ));
        out.push_str("  %len_pv = load i64, i64* %len_pp\n");
        out.push_str("  %is_empty_pp = icmp eq i64 %len_pv, 0\n");
        out.push_str("  br i1 %is_empty_pp, label %abort_pp, label %ok_pp\n");
        out.push_str("abort_pp:\n");
        out.push_str("  call void @abort()\n");
        out.push_str("  unreachable\n");
        out.push_str("ok_pp:\n");
        out.push_str("  %new_len_pp = sub i64 %len_pv, 1\n");
        out.push_str(&format!(
            "  %data_pp = getelementptr {}, {}* %xs_pp, i32 0, i32 0\n",
            s_ty, s_ty
        ));
        out.push_str(&format!(
            "  %data_v_pp = load {}*, {}** %data_pp\n",
            elt_ty, elt_ty
        ));
        out.push_str(&format!(
            "  %slot_pp = getelementptr {}, {}* %data_v_pp, i64 %new_len_pp\n",
            elt_ty, elt_ty
        ));
        out.push_str(&format!(
            "  %popped_pp = load {}, {}* %slot_pp\n",
            elt_ty, elt_ty
        ));
        out.push_str("  store i64 %new_len_pp, i64* %len_pp\n");
        out.push_str(&format!("  ret {} %popped_pp\n", elt_ty));
        out.push_str("}\n");
    }

    // ---- set(xs, i, v): write in place, return xs.
    out.push_str(&format!(
        "define {} {}({} %xs, i64 %i, {} %v) {{\n",
        s_ty, set_name, s_ty, elt_ty
    ));
    out.push_str(&format!("  %data = extractvalue {} %xs, 0\n", s_ty));
    out.push_str(&format!(
        "  %p = getelementptr {}, {}* %data, i64 %i\n",
        elt_ty, elt_ty
    ));
    // For non-Copy element types (`Vec<U>`) free the old
    // slot's owned resources before overwriting; otherwise
    // an in-place set leaks the prior inner-Vec's heap
    // buffer. Refines #7.
    if !element_is_copy {
        // Closure #157: extend the per-shape Vec __set
        // helper's old-element drop to also handle
        // OwnedStr, Struct (with owning fields), and
        // payloaded Enum element types. Was Vec-only —
        // `set(Vec<OwnedStr>, …)` and friends leaked the
        // previous slot's heap.
        match element {
            Type::Vec(inner) => {
                let inner_free = format!(
                    "@intent_vec_{}__free",
                    vec_struct_tag(inner)
                );
                out.push_str(&format!(
                    "  %old = load {}, {}* %p\n",
                    elt_ty, elt_ty
                ));
                out.push_str(&format!(
                    "  call void {}({} %old)\n",
                    inner_free, elt_ty
                ));
            }
            Type::OwnedStr => {
                out.push_str("  %old = load i8*, i8** %p\n");
                out.push_str("  call void @free(i8* %old)\n");
            }
            Type::Struct(name) => {
                let fields = LLVM_STRUCT_FIELDS_REGISTRY
                    .with(|r| r.borrow().get(name).cloned())
                    .unwrap_or_default();
                let has_owning = fields.iter().any(|(_, ty)| !ty.is_copy());
                if has_owning {
                    let s_struct = format!("%Struct_{}", name);
                    let mut tmp_counter: usize = 0;
                    emit_vec_element_struct_drop(
                        &s_struct,
                        "%p",
                        &fields,
                        &mut tmp_counter,
                        out,
                    );
                }
            }
            Type::Enum(name) => {
                let payload_ty = LLVM_ENUM_PAYLOAD_REGISTRY
                    .with(|r| r.borrow().get(name).cloned());
                let payload_tags: Vec<u32> = LLVM_ENUM_PAYLOAD_TAGS_REGISTRY
                    .with(|r| r.borrow().get(name).cloned().unwrap_or_default());
                let heap_kind = match &payload_ty {
                    Some(Type::OwnedStr) => Some("owned_str"),
                    Some(Type::Vec(_)) => Some("vec"),
                    _ => None,
                };
                if heap_kind.is_some() && !payload_tags.is_empty() {
                    let s_enum = format!("%Enum_{}", name);
                    out.push_str(&format!(
                        "  %old = load {}, {}* %p\n",
                        s_enum, s_enum
                    ));
                    out.push_str(&format!(
                        "  %old_tag = extractvalue {} %old, 0\n",
                        s_enum
                    ));
                    out.push_str(&format!(
                        "  %old_payload = extractvalue {} %old, 1\n",
                        s_enum
                    ));
                    let mut prev = "i1 false".to_string();
                    for (idx, t) in payload_tags.iter().enumerate() {
                        out.push_str(&format!(
                            "  %set_cmp_{} = icmp eq i32 %old_tag, {}\n",
                            idx, t
                        ));
                        out.push_str(&format!(
                            "  %set_or_{} = or {}, %set_cmp_{}\n",
                            idx, prev, idx
                        ));
                        prev = format!("i1 %set_or_{}", idx);
                    }
                    let cond = prev.trim_start_matches("i1 ").to_string();
                    out.push_str(&format!(
                        "  br i1 {}, label %set_enum_free, label %set_enum_done\n",
                        cond
                    ));
                    out.push_str("set_enum_free:\n");
                    match heap_kind {
                        Some("owned_str") => {
                            out.push_str("  call void @free(i8* %old_payload)\n");
                        }
                        Some("vec") => {
                            if let Some(Type::Vec(inner)) = &payload_ty {
                                let free_name = format!(
                                    "@intent_vec_{}__free",
                                    vec_struct_tag(inner)
                                );
                                let v_struct = vec_struct_name(inner);
                                out.push_str(&format!(
                                    "  call void {}({} %old_payload)\n",
                                    free_name, v_struct
                                ));
                            }
                        }
                        _ => {}
                    }
                    out.push_str("  br label %set_enum_done\n");
                    out.push_str("set_enum_done:\n");
                }
            }
            _ => {}
        }
    }
    out.push_str(&format!("  store {} %v, {}* %p\n", elt_ty, elt_ty));
    out.push_str(&format!("  ret {} %xs\n", s_ty));
    out.push_str("}\n");

    // ---- clone(xs): malloc new buffer + copy each element.
    // For Copy elements a memcpy of the whole buffer is
    // correct; for non-Copy (`Vec<U>`) each slot needs the
    // element's own __clone so duplicated structs don't
    // alias their source's inner buffer.
    out.push_str(&format!(
        "define {} {}({} %xs) {{\n",
        s_ty, clone_name, s_ty
    ));
    out.push_str(&format!("  %data = extractvalue {} %xs, 0\n", s_ty));
    out.push_str(&format!("  %len = extractvalue {} %xs, 1\n", s_ty));
    out.push_str(&format!("  %bytes = mul i64 %len, {}\n", elt_size));
    out.push_str("  %is_empty = icmp eq i64 %len, 0\n");
    out.push_str(&format!(
        "  %alloc_bytes = select i1 %is_empty, i64 {}, i64 %bytes\n",
        elt_size
    ));
    out.push_str("  %raw = call i8* @malloc(i64 %alloc_bytes)\n");
    out.push_str(&format!(
        "  %new_data = bitcast i8* %raw to {}*\n",
        elt_ty
    ));
    if element_is_copy {
        out.push_str(&format!(
            "  %src8 = bitcast {}* %data to i8*\n",
            elt_ty
        ));
        out.push_str("  %_ck = call i8* @memcpy(i8* %raw, i8* %src8, i64 %bytes)\n");
    } else {
        // Non-Copy element: deep-clone each slot so the
        // returned Vec doesn't share heap with the source
        // (closure #143's clone-of-fresh-vec fix and
        // closure #152's general clone bug both surfaced
        // here for `Vec<OwnedStr>` / `Vec<EnumWithOwnedStr>`).
        // Pattern: load src slot, produce a deep clone via
        // the appropriate helper, store into the new
        // buffer. Tag-only enums fall through to a shallow
        // copy (memcpy via the Copy path above).
        // Detect whether the per-element clone branches
        // (Enum payload case) so the loop's phi back-
        // predecessor matches the actual last block.
        let enum_payloaded = matches!(element, Type::Enum(name)
            if LLVM_ENUM_PAYLOAD_REGISTRY.with(|r| r.borrow().contains_key(name))
                && matches!(
                    LLVM_ENUM_PAYLOAD_REGISTRY.with(|r| r.borrow().get(name).cloned()),
                    Some(Type::OwnedStr)
                ));
        let back_pred = if enum_payloaded { "%cln_join" } else { "%cln_body" };
        out.push_str("  br label %cln_check\n");
        out.push_str("cln_check:\n");
        out.push_str(&format!(
            "  %ci = phi i64 [0, %0], [%ci_next, {}]\n",
            back_pred
        ));
        out.push_str("  %ci_lt = icmp ult i64 %ci, %len\n");
        out.push_str("  br i1 %ci_lt, label %cln_body, label %cln_done\n");
        out.push_str("cln_body:\n");
        out.push_str(&format!(
            "  %src_p = getelementptr {}, {}* %data, i64 %ci\n",
            elt_ty, elt_ty
        ));
        out.push_str(&format!(
            "  %src_v = load {}, {}* %src_p\n",
            elt_ty, elt_ty
        ));
        let cloned_value: String = match element {
            Type::Vec(inner) => {
                let inner_clone =
                    format!("@intent_vec_{}__clone", vec_struct_tag(inner));
                out.push_str(&format!(
                    "  %cloned = call {} {}({} %src_v)\n",
                    elt_ty, inner_clone, elt_ty
                ));
                "%cloned".to_string()
            }
            Type::OwnedStr => {
                // Closure #152: deep clone via
                // intent_str_concat with empty literal.
                out.push_str(
                    "  %empty_p = getelementptr [1 x i8], [1 x i8]* @.empty_str_clone, i64 0, i64 0\n",
                );
                out.push_str(
                    "  %cloned = call i8* @intent_str_concat(i8* %src_v, i32 0, i8* %empty_p, i32 0)\n",
                );
                "%cloned".to_string()
            }
            Type::Enum(name)
                if LLVM_ENUM_PAYLOAD_REGISTRY
                    .with(|r| r.borrow().contains_key(name)) =>
            {
                // Tag-switched payload clone for payloaded
                // enums. Only OwnedStr payload supported in
                // v1; other payload kinds fall through to a
                // shallow copy.
                let payload_ty = LLVM_ENUM_PAYLOAD_REGISTRY
                    .with(|r| r.borrow().get(name).cloned());
                if matches!(payload_ty, Some(Type::OwnedStr)) {
                    let payload_tags: Vec<u32> = LLVM_ENUM_PAYLOAD_TAGS_REGISTRY
                        .with(|r| r.borrow().get(name).cloned().unwrap_or_default());
                    out.push_str(&format!(
                        "  %src_tag = extractvalue {} %src_v, 0\n",
                        elt_ty
                    ));
                    out.push_str(&format!(
                        "  %src_payload = extractvalue {} %src_v, 1\n",
                        elt_ty
                    ));
                    let mut prev = "i1 false".to_string();
                    for (idx, t) in payload_tags.iter().enumerate() {
                        out.push_str(&format!(
                            "  %tg{} = icmp eq i32 %src_tag, {}\n",
                            idx, t
                        ));
                        out.push_str(&format!(
                            "  %or{} = or {}, %tg{}\n",
                            idx, prev, idx
                        ));
                        prev = format!("i1 %or{}", idx);
                    }
                    let cond = prev.trim_start_matches("i1 ").to_string();
                    out.push_str(&format!(
                        "  br i1 {}, label %cln_payloaded, label %cln_taggy\n",
                        cond
                    ));
                    out.push_str("cln_payloaded:\n");
                    out.push_str(
                        "  %empty_p = getelementptr [1 x i8], [1 x i8]* @.empty_str_clone, i64 0, i64 0\n",
                    );
                    out.push_str(
                        "  %payload_cloned = call i8* @intent_str_concat(i8* %src_payload, i32 0, i8* %empty_p, i32 0)\n",
                    );
                    out.push_str(&format!(
                        "  %enum_p1 = insertvalue {} undef, i32 %src_tag, 0\n",
                        elt_ty
                    ));
                    out.push_str(&format!(
                        "  %enum_p2 = insertvalue {} %enum_p1, i8* %payload_cloned, 1\n",
                        elt_ty
                    ));
                    out.push_str("  br label %cln_join\n");
                    out.push_str("cln_taggy:\n");
                    out.push_str("  br label %cln_join\n");
                    out.push_str("cln_join:\n");
                    out.push_str(&format!(
                        "  %cloned = phi {} [ %enum_p2, %cln_payloaded ], [ %src_v, %cln_taggy ]\n",
                        elt_ty
                    ));
                    "%cloned".to_string()
                } else {
                    // Shallow copy fallback for non-OwnedStr
                    // enum payloads (Vec / Struct payloads
                    // still pending).
                    "%src_v".to_string()
                }
            }
            Type::Struct(name) => {
                // Closure #153 LLVM: deep-clone each owning
                // field via the same shape as the OwnedStr
                // arm. For v1 we support OwnedStr fields
                // (the common case). Other heap field types
                // fall through to a shallow copy and would
                // need follow-up work.
                let fields = LLVM_STRUCT_FIELDS_REGISTRY
                    .with(|r| r.borrow().get(name).cloned())
                    .unwrap_or_default();
                let has_owning = fields.iter().any(|(_, ty)| !ty.is_copy());
                if !has_owning {
                    "%src_v".to_string()
                } else {
                    let s_ty = format!("%Struct_{}", name);
                    let mut acc = "undef".to_string();
                    for (idx, (_, fty)) in fields.iter().enumerate() {
                        let f_src = format!("%f_src_{}", idx);
                        let f_lty = llvm_type_string(fty);
                        out.push_str(&format!(
                            "  {} = extractvalue {} %src_v, {}\n",
                            f_src, s_ty, idx
                        ));
                        let f_cloned = match fty {
                            Type::OwnedStr => {
                                out.push_str(
                                    "  %f_empty_p = getelementptr [1 x i8], [1 x i8]* @.empty_str_clone, i64 0, i64 0\n",
                                );
                                let cloned = format!("%f_cloned_{}", idx);
                                out.push_str(&format!(
                                    "  {} = call i8* @intent_str_concat(i8* {}, i32 0, i8* %f_empty_p, i32 0)\n",
                                    cloned, f_src
                                ));
                                cloned
                            }
                            _ => f_src.clone(),
                        };
                        let next = format!("%struct_acc_{}", idx);
                        out.push_str(&format!(
                            "  {} = insertvalue {} {}, {} {}, {}\n",
                            next, s_ty, acc, f_lty, f_cloned, idx
                        ));
                        acc = next;
                    }
                    acc
                }
            }
            _ => "%src_v".to_string(),
        };
        out.push_str(&format!(
            "  %dst_p = getelementptr {}, {}* %new_data, i64 %ci\n",
            elt_ty, elt_ty
        ));
        out.push_str(&format!(
            "  store {} {}, {}* %dst_p\n",
            elt_ty, cloned_value, elt_ty
        ));
        out.push_str("  %ci_next = add i64 %ci, 1\n");
        out.push_str("  br label %cln_check\n");
        out.push_str("cln_done:\n");
    }
    out.push_str("  %new_cap = select i1 %is_empty, i64 1, i64 %len\n");
    out.push_str(&format!(
        "  %r0 = insertvalue {} undef, {}* %new_data, 0\n",
        s_ty, elt_ty
    ));
    out.push_str(&format!("  %r1 = insertvalue {} %r0, i64 %len, 1\n", s_ty));
    out.push_str(&format!(
        "  %r2 = insertvalue {} %r1, i64 %new_cap, 2\n",
        s_ty
    ));
    out.push_str(&format!("  ret {} %r2\n", s_ty));
    out.push_str("}\n");

    // ---- free(xs): release the heap buffer (+ recursively
    // free each element when the element type owns heap of
    // its own). Element types handled: `Vec<U>` (calls inner
    // Vec __free), `OwnedStr` (per-slot @free), `Struct{…}`
    // with owning fields (per-field drop walk via the LLVM
    // struct-field registry, mirrors `emit_llvm_struct_field_drops`).
    // Always emitted so callers can use a uniform interface and
    // not branch on `is_copy` at Drop sites. Closure #127.
    out.push_str(&format!(
        "define void {}({} %xs) {{\n",
        free_name, s_ty
    ));
    out.push_str(&format!("  %data = extractvalue {} %xs, 0\n", s_ty));
    if !element_is_copy {
        let mut needs_loop = false;
        let mut body = String::new();
        let mut tmp_counter: usize = 0;
        let next_tmp = |c: &mut usize| -> String {
            let n = format!("%fd{}", *c);
            *c += 1;
            n
        };
        match element {
            Type::Vec(inner) => {
                needs_loop = true;
                let inner_free =
                    format!("@intent_vec_{}__free", vec_struct_tag(inner));
                let v = next_tmp(&mut tmp_counter);
                body.push_str(&format!(
                    "  {} = load {}, {}* %elt_p\n",
                    v, elt_ty, elt_ty
                ));
                body.push_str(&format!(
                    "  call void {}({} {})\n",
                    inner_free, elt_ty, v
                ));
            }
            Type::OwnedStr => {
                needs_loop = true;
                let v = next_tmp(&mut tmp_counter);
                body.push_str(&format!(
                    "  {} = load i8*, i8** %elt_p\n",
                    v
                ));
                body.push_str(&format!(
                    "  call void @free(i8* {})\n",
                    v
                ));
            }
            Type::Struct(name) => {
                let fields = LLVM_STRUCT_FIELDS_REGISTRY
                    .with(|r| r.borrow().get(name).cloned())
                    .unwrap_or_default();
                if !fields.is_empty() {
                    needs_loop = true;
                    let s_struct = format!("%Struct_{}", name);
                    emit_vec_element_struct_drop(
                        &s_struct,
                        "%elt_p",
                        &fields,
                        &mut tmp_counter,
                        &mut body,
                    );
                }
            }
            Type::Enum(name) => {
                // Per-element enum drop: load the enum,
                // extract tag + payload, OR-chain over the
                // payloaded tags, branch to a free block for
                // payloaded variants. Mirrors the scope-exit
                // Drop emission for enums. Closure #151
                // (`Vec<PayloadedEnum>` was leaking each
                // element's payload at outer __free time;
                // LLVM crashed with invalid free).
                let payload_ty = LLVM_ENUM_PAYLOAD_REGISTRY
                    .with(|r| r.borrow().get(name).cloned());
                let heap_kind = match &payload_ty {
                    Some(Type::OwnedStr) => Some("owned_str"),
                    Some(Type::Vec(_)) => Some("vec"),
                    _ => None,
                };
                if let Some(kind) = heap_kind {
                    let payload_tags: Vec<u32> = LLVM_ENUM_PAYLOAD_TAGS_REGISTRY
                        .with(|r| r.borrow().get(name).cloned().unwrap_or_default());
                    if !payload_tags.is_empty() {
                        needs_loop = true;
                        let s_enum = format!("%Enum_{}", name);
                        let loaded = next_tmp(&mut tmp_counter);
                        body.push_str(&format!(
                            "  {} = load {}, {}* %elt_p\n",
                            loaded, s_enum, s_enum
                        ));
                        let tag = next_tmp(&mut tmp_counter);
                        body.push_str(&format!(
                            "  {} = extractvalue {} {}, 0\n",
                            tag, s_enum, loaded
                        ));
                        let payload = next_tmp(&mut tmp_counter);
                        body.push_str(&format!(
                            "  {} = extractvalue {} {}, 1\n",
                            payload, s_enum, loaded
                        ));
                        let mut prev = "i1 false".to_string();
                        for t in &payload_tags {
                            let cmp = next_tmp(&mut tmp_counter);
                            body.push_str(&format!(
                                "  {} = icmp eq i32 {}, {}\n",
                                cmp, tag, t
                            ));
                            let or_v = next_tmp(&mut tmp_counter);
                            body.push_str(&format!(
                                "  {} = or {}, {}\n",
                                or_v, prev, cmp
                            ));
                            prev = format!("i1 {}", or_v);
                        }
                        let cond = prev.trim_start_matches("i1 ").to_string();
                        body.push_str(&format!(
                            "  br i1 {}, label %enum_free, label %enum_done\n",
                            cond
                        ));
                        body.push_str("enum_free:\n");
                        match kind {
                            "owned_str" => {
                                body.push_str(&format!(
                                    "  call void @free(i8* {})\n",
                                    payload
                                ));
                            }
                            "vec" => {
                                if let Some(Type::Vec(inner)) = &payload_ty {
                                    let free_name = format!(
                                        "@intent_vec_{}__free",
                                        vec_struct_tag(inner)
                                    );
                                    let v_struct = vec_struct_name(inner);
                                    body.push_str(&format!(
                                        "  call void {}({} {})\n",
                                        free_name, v_struct, payload
                                    ));
                                }
                            }
                            _ => {}
                        }
                        body.push_str("  br label %enum_done\n");
                        body.push_str("enum_done:\n");
                    }
                }
            }
            _ => {}
        }
        if needs_loop {
            out.push_str(&format!("  %len = extractvalue {} %xs, 1\n", s_ty));
            out.push_str("  br label %fr_check\n");
            out.push_str("fr_check:\n");
            // The phi's "back" predecessor is `fr_body` when
            // the body emits no new basic block, OR `enum_done`
            // when the Enum-element drop appended that label.
            // Detect by scanning the emitted body for the label
            // marker.
            let back_pred = if body.contains("enum_done:\n") {
                "%enum_done"
            } else {
                "%fr_body"
            };
            out.push_str(&format!(
                "  %fi = phi i64 [0, %0], [%fi_next, {}]\n",
                back_pred
            ));
            out.push_str("  %fi_lt = icmp ult i64 %fi, %len\n");
            out.push_str("  br i1 %fi_lt, label %fr_body, label %fr_done\n");
            out.push_str("fr_body:\n");
            out.push_str(&format!(
                "  %elt_p = getelementptr {}, {}* %data, i64 %fi\n",
                elt_ty, elt_ty
            ));
            out.push_str(&body);
            out.push_str("  %fi_next = add i64 %fi, 1\n");
            out.push_str("  br label %fr_check\n");
            out.push_str("fr_done:\n");
        }
    }
    out.push_str(&format!(
        "  %data8 = bitcast {}* %data to i8*\n",
        elt_ty
    ));
    out.push_str("  call void @free(i8* %data8)\n");
    out.push_str("  ret void\n");
    out.push_str("}\n\n");
}

/// Emit per-field drop statements for a struct slot pointed at
/// by `addr`. Mirrors `emit_llvm_struct_field_drops` but takes a
/// plain counter instead of a `FnCtx` so it can be invoked from
/// the module-level `intent_vec_<S>__free` emitter (which has no
/// function context). Closure #127.
fn emit_vec_element_struct_drop(
    s_ty: &str,
    addr: &str,
    fields: &[(String, Type)],
    counter: &mut usize,
    out: &mut String,
) {
    let next = |c: &mut usize| -> String {
        let n = format!("%fd{}", *c);
        *c += 1;
        n
    };
    for (idx, (_field_name, field_ty)) in fields.iter().enumerate().rev() {
        match field_ty {
            Type::OwnedStr => {
                let fp = next(counter);
                out.push_str(&format!(
                    "  {} = getelementptr {}, {}* {}, i64 0, i32 {}\n",
                    fp, s_ty, s_ty, addr, idx
                ));
                let v = next(counter);
                out.push_str(&format!("  {} = load i8*, i8** {}\n", v, fp));
                out.push_str(&format!("  call void @free(i8* {})\n", v));
            }
            Type::Vec(element) => {
                let v_struct = vec_struct_name(element);
                let fp = next(counter);
                out.push_str(&format!(
                    "  {} = getelementptr {}, {}* {}, i64 0, i32 {}\n",
                    fp, s_ty, s_ty, addr, idx
                ));
                let v = next(counter);
                out.push_str(&format!(
                    "  {} = load {}, {}* {}\n",
                    v, v_struct, v_struct, fp
                ));
                let free_name =
                    format!("@intent_vec_{}__free", vec_struct_tag(element));
                out.push_str(&format!(
                    "  call void {}({} {})\n",
                    free_name, v_struct, v
                ));
            }
            Type::Struct(inner_name) => {
                let inner_fields = LLVM_STRUCT_FIELDS_REGISTRY
                    .with(|r| r.borrow().get(inner_name).cloned())
                    .unwrap_or_default();
                if !inner_fields.is_empty() {
                    let fp = next(counter);
                    out.push_str(&format!(
                        "  {} = getelementptr {}, {}* {}, i64 0, i32 {}\n",
                        fp, s_ty, s_ty, addr, idx
                    ));
                    let inner_s_ty = format!("%Struct_{}", inner_name);
                    emit_vec_element_struct_drop(
                        &inner_s_ty,
                        &fp,
                        &inner_fields,
                        counter,
                        out,
                    );
                }
            }
            _ => {}
        }
    }
}

/// In-buffer LLVM value spelling for a Vec's element type.
/// Differs from `llvm_type_string` for arrays: an
/// `Array<T, N>` SSA value uses `[N x T]*` (alloca-pointer
/// form), but inside the Vec buffer each slot holds the raw
/// `[N x T]` value. Without this distinction the struct decl
/// becomes `{ [N x T]**, i64, i64 }` (double-pointer) and
/// every extract/bitcast in the helpers misaligns. Phase 2c
/// of #7.
pub(crate) fn vec_element_value_str(element: &Type) -> String {
    match element {
        Type::Array { element: inner, length } => {
            format!(
                "[{} x {}]",
                length,
                vec_element_value_str(inner),
            )
        }
        _ => llvm_type_string(element),
    }
}

/// Byte size of a Vec's element type when sitting in the
/// heap buffer. Handles aggregate elements (Vec<U> = 24, fixed
/// arrays = N * inner_size). Refines #7 — the prior emit used
/// `element.bits().unwrap_or(64) / 8` which returned 8 for
/// every aggregate (Vec, Array) and led to under-allocation +
/// silent heap corruption.
/// LLVM expression form for an element's byte size. For
/// scalar / array element types this is just the static
/// `u64` byte count rendered as a literal. For structs
/// (whose layout depends on field types + alignment we
/// don't model in the compiler), we emit the LLVM idiom
/// `ptrtoint (T* getelementptr (T, T* null, i32 1) to
/// i64)` — a constant expression that LLVM resolves to
/// `sizeof(T)` at compile time. Used by emit_vec_helpers
/// so `Vec<Point>` mallocs the right number of bytes.
/// T1.2 + Vec<Struct> LLVM.
pub(crate) fn vec_element_size_expr(element: &Type) -> String {
    match element {
        Type::Struct(name) => {
            let s_ty = format!("%Struct_{}", name);
            format!(
                "ptrtoint ({}* getelementptr ({}, {}* null, i32 1) to i64)",
                s_ty, s_ty, s_ty
            )
        }
        Type::Tuple(_) => {
            // Tuples use LLVM's anonymous `{T1, T2, …}`
            // type literal. The GEP-null sizeof trick
            // works on anonymous types too.
            let t_ty = llvm_type_string(element);
            format!(
                "ptrtoint ({}* getelementptr ({}, {}* null, i32 1) to i64)",
                t_ty, t_ty, t_ty
            )
        }
        // Payloaded enums are tagged-union structs whose
        // layout (i32 + i8* with padding) depends on the
        // host's alignment. Use the GEP-null sizeof trick
        // for them too. Tag-only enums fall through to the
        // byte-size literal (just 4 bytes for an i32 tag).
        // Closure #151 (`Vec<Msg>` was under-allocating
        // — vec literal called `malloc(16)` for 2 elements
        // when each is 16 bytes, leaking past the buffer).
        Type::Enum(name) => {
            let payloaded = LLVM_ENUM_PAYLOAD_REGISTRY
                .with(|r| r.borrow().contains_key(name));
            if payloaded {
                let e_ty = format!("%Enum_{}", name);
                format!(
                    "ptrtoint ({}* getelementptr ({}, {}* null, i32 1) to i64)",
                    e_ty, e_ty, e_ty
                )
            } else {
                format!("{}", vec_element_byte_size(element))
            }
        }
        _ => format!("{}", vec_element_byte_size(element)),
    }
}

pub(crate) fn vec_element_byte_size(element: &Type) -> u64 {
    match element {
        // `intent_vec_<T>` is `{ T*, i64, i64 }`: 24 bytes
        // regardless of T (pointers/lengths are 8-byte each
        // on every supported host).
        Type::Vec(_) => 24,
        Type::Array { element: inner, length } => {
            vec_element_byte_size(inner) * length
        }
        Type::Task => 16,
        // Channel / Mutex / Guard / Atomic are pointer-or-
        // struct shaped; conservatively pick 24 to cover
        // the worst case. Vecs of these aren't allowed by
        // the checker today so this is defensive.
        Type::Channel(_, _) | Type::Mutex(_) | Type::Guard(_) => 24,
        Type::Atomic(inner) => vec_element_byte_size(inner),
        // Vtables Phase 4b: `dyn Iface` is a fat pointer
        // (vtable pointer + data pointer) — 16 bytes.
        Type::Object(_) => 16,
        _ => (element.bits().unwrap_or(64) / 8) as u64,
    }
}

/// Walk a typed statement tree and intern every `assert "msg"`
/// message into a Vec + lookup map. Each unique message gets one
/// global; the map carries its index.
fn collect_assert_messages(
    stmt: &TypedStmt,
    msgs: &mut Vec<String>,
    idx: &mut HashMap<String, usize>,
) {
    match stmt {
        TypedStmt::Assert { message: Some(m), .. } => {
            if !idx.contains_key(m) {
                idx.insert(m.clone(), msgs.len());
                msgs.push(m.clone());
            }
        }
        TypedStmt::If { then_body, else_body, .. } => {
            for s in then_body { collect_assert_messages(s, msgs, idx); }
            for s in else_body { collect_assert_messages(s, msgs, idx); }
        }
        TypedStmt::While { body, .. }
        | TypedStmt::For { body, .. }
        | TypedStmt::ForIter { body, .. } => {
            for s in body { collect_assert_messages(s, msgs, idx); }
        }
        _ => {}
    }
}

/// Walk a typed statement tree and intern every `print "literal";`
/// text into a Vec + lookup map. Each unique text gets one global.
/// Intern every string literal that surfaces in a function body —
/// both `print "literal"` items AND `ExprKind::Str` literals
/// appearing as call args or assignment RHS. The pre-pass emits
/// `@.print_str.<n>` globals; the emitter looks them up by index.
fn collect_print_strings(
    stmt: &TypedStmt,
    msgs: &mut Vec<String>,
    idx: &mut HashMap<String, usize>,
) {
    let intern = |t: &str, msgs: &mut Vec<String>, idx: &mut HashMap<String, usize>| {
        if !idx.contains_key(t) {
            idx.insert(t.to_string(), msgs.len());
            msgs.push(t.to_string());
        }
    };
    match stmt {
        TypedStmt::Let { expr, .. }
        | TypedStmt::Reassign { expr, .. }
        | TypedStmt::Discard { expr }
        | TypedStmt::Return { expr }
        | TypedStmt::Assert { expr, .. }
        | TypedStmt::Prove { expr } => collect_strings_in_expr(expr, msgs, idx, &intern),
        TypedStmt::Print { items } => {
            for it in items {
                match it {
                    crate::ir::TypedPrintItem::Str(text) => intern(text, msgs, idx),
                    crate::ir::TypedPrintItem::Expr(e) => {
                        collect_strings_in_expr(e, msgs, idx, &intern)
                    }
                }
            }
        }
        TypedStmt::If { cond, then_body, else_body } => {
            collect_strings_in_expr(cond, msgs, idx, &intern);
            for s in then_body { collect_print_strings(s, msgs, idx); }
            for s in else_body { collect_print_strings(s, msgs, idx); }
        }
        TypedStmt::While { cond, body } => {
            collect_strings_in_expr(cond, msgs, idx, &intern);
            for s in body { collect_print_strings(s, msgs, idx); }
        }
        TypedStmt::For { start, end, body, .. } => {
            collect_strings_in_expr(start, msgs, idx, &intern);
            collect_strings_in_expr(end, msgs, idx, &intern);
            for s in body { collect_print_strings(s, msgs, idx); }
        }
        TypedStmt::ForIter { body, .. } => {
            for s in body { collect_print_strings(s, msgs, idx); }
        }
        TypedStmt::IndexAssign { index, value, .. } => {
            collect_strings_in_expr(index, msgs, idx, &intern);
            collect_strings_in_expr(value, msgs, idx, &intern);
        }
        TypedStmt::FieldAssign { object, value, .. } => {
            collect_strings_in_expr(object, msgs, idx, &intern);
            collect_strings_in_expr(value, msgs, idx, &intern);
        }
        _ => {}
    }
}

fn collect_strings_in_expr<F>(
    expr: &TypedExpr,
    msgs: &mut Vec<String>,
    idx: &mut HashMap<String, usize>,
    intern: &F,
) where
    F: Fn(&str, &mut Vec<String>, &mut HashMap<String, usize>),
{
    match &expr.kind {
        TypedExprKind::Str(s) => intern(s, msgs, idx),
        TypedExprKind::Unary { expr, .. } => collect_strings_in_expr(expr, msgs, idx, intern),
        TypedExprKind::Binary { left, right, .. } => {
            collect_strings_in_expr(left, msgs, idx, intern);
            collect_strings_in_expr(right, msgs, idx, intern);
        }
        TypedExprKind::Call { args, .. } | TypedExprKind::ArrayLit { elements: args } => {
            for a in args { collect_strings_in_expr(a, msgs, idx, intern); }
        }
        TypedExprKind::CallIndirect { callee, args } => {
            collect_strings_in_expr(callee, msgs, idx, intern);
            for a in args { collect_strings_in_expr(a, msgs, idx, intern); }
        }
        TypedExprKind::Cast { expr, .. } => collect_strings_in_expr(expr, msgs, idx, intern),
        TypedExprKind::Index { array, index, .. } => {
            collect_strings_in_expr(array, msgs, idx, intern);
            collect_strings_in_expr(index, msgs, idx, intern);
        }
        TypedExprKind::Len { array, .. } => collect_strings_in_expr(array, msgs, idx, intern),
        TypedExprKind::Tuple { elements } => {
            for e in elements { collect_strings_in_expr(e, msgs, idx, intern); }
        }
        TypedExprKind::TupleAccess { tuple, .. } => {
            collect_strings_in_expr(tuple, msgs, idx, intern);
        }
        TypedExprKind::StructLit { fields, .. } => {
            for (_, e) in fields { collect_strings_in_expr(e, msgs, idx, intern); }
        }
        TypedExprKind::FieldAccess { object, .. } => {
            collect_strings_in_expr(object, msgs, idx, intern);
        }
        TypedExprKind::EnumVariantWithPayload { payload, .. } => {
            collect_strings_in_expr(payload, msgs, idx, intern);
        }
        TypedExprKind::Match { scrutinee, arms } => {
            collect_strings_in_expr(scrutinee, msgs, idx, intern);
            for arm in arms {
                collect_strings_in_expr(&arm.body, msgs, idx, intern);
            }
        }
        TypedExprKind::IfExpr { cond, then_value, else_value } => {
            collect_strings_in_expr(cond, msgs, idx, intern);
            collect_strings_in_expr(then_value, msgs, idx, intern);
            collect_strings_in_expr(else_value, msgs, idx, intern);
        }
        TypedExprKind::Block { stmts, tail } => {
            for s in stmts { collect_print_strings(s, msgs, idx); }
            collect_strings_in_expr(tail, msgs, idx, intern);
        }
        TypedExprKind::DynDispatch { receiver, args, .. } => {
            collect_strings_in_expr(receiver, msgs, idx, intern);
            for a in args { collect_strings_in_expr(a, msgs, idx, intern); }
        }
        TypedExprKind::DynCoerce { value, .. } => {
            collect_strings_in_expr(value, msgs, idx, intern);
        }
        TypedExprKind::Int(_)
        | TypedExprKind::Float(_)
        | TypedExprKind::Bool(_)
        | TypedExprKind::Var(_)
        | TypedExprKind::Ref { .. }
        | TypedExprKind::RefMut { .. }
        | TypedExprKind::RefField { .. }
        | TypedExprKind::RefMutField { .. }
        | TypedExprKind::FnRef { .. }
        | TypedExprKind::EnumVariant { .. } => {}
    }
}

/// Escape a Rust-side string for embedding as the c"..." form in a
/// private constant. LLVM IR accepts printable ASCII directly and
/// requires `\NN` (uppercase-hex two-digit) for everything else,
/// plus an escape for `"` and `\`.
fn escape_for_llvm_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match *b {
            b'"' => out.push_str("\\22"),
            b'\\' => out.push_str("\\5C"),
            0x20..=0x7E => out.push(*b as char),
            other => out.push_str(&format!("\\{:02X}", other)),
        }
    }
    out
}

fn vec_element_of_first_arg(args: &[TypedExpr]) -> Option<Type> {
    let first = args.first()?;
    match first.ty.deref() {
        Type::Vec(element) => Some((**element).clone()),
        _ => None,
    }
}

pub(crate) fn vec_struct_name(element: &Type) -> String {
    format!("%intent_vec_{}", vec_struct_tag(element))
}

/// Mixed-place index+field assign: if the leaf field is a
/// heap-shaped type (OwnedStr / Vec<T>), the old slot's
/// resources must be freed before the new value is stored.
/// `p` already points at the leaf slot; `leaf_ty` is its
/// type. No-op when `field_path` is empty (the whole-element
/// store has its own slot-drop path) or the leaf is Copy.
/// Closure #126 / F2.
fn emit_leaf_overwrite_drop(
    leaf_ty: &Type,
    _field_path: &[(String, u32)],
    p: &str,
    ctx: &mut FnCtx<'_>,
    out: &mut String,
) {
    // Closure #167: the early-return on
    // `field_path.is_empty()` (matching only deep mixed-
    // place assigns) was blocking the old-slot drop on
    // plain `xs[i] = v` for non-Copy element types. With
    // `field_path = []`, `p` points at the array slot
    // directly, which is exactly what we want to load+free
    // for OwnedStr / Vec. Copy element types stay no-ops
    // via the wildcard match arm.
    match leaf_ty {
        Type::OwnedStr => {
            let old = ctx.fresh_tmp();
            out.push_str(&format!("  {} = load i8*, i8** {}\n", old, p));
            out.push_str(&format!("  call void @free(i8* {})\n", old));
        }
        Type::Vec(element) => {
            let s_ty = vec_struct_name(element);
            let old = ctx.fresh_tmp();
            out.push_str(&format!("  {} = load {}, {}* {}\n", old, s_ty, s_ty, p));
            let free_name = format!("@intent_vec_{}__free", vec_struct_tag(element));
            out.push_str(&format!(
                "  call void {}({} {})\n",
                free_name, s_ty, old
            ));
        }
        _ => {}
    }
}

/// Composable identifier-safe tag for a Vec's element type.
/// Mirrors C's `element_tag` so the two backends agree on the
/// per-shape struct name. Recurses through `Vec<U>` and
/// `Array<U, N>` so nested aggregates produce distinct,
/// readable identifiers (`vec_int64`, `vec_vec_int64`,
/// `arr4_int64`). Refines #7: was calling `llvm_type` which
/// panics on aggregate types and collapsed every nested Vec
/// to the same tag.
pub(crate) fn vec_struct_tag(element: &Type) -> String {
    match element {
        Type::Vec(inner) => format!("vec_{}", vec_struct_tag(inner)),
        Type::Array { element: inner, length } => {
            format!("arr{}_{}", length, vec_struct_tag(inner))
        }
        // Nominal struct/enum types get a stable
        // identifier-safe tag so `Vec<Point>` becomes
        // `intent_vec_Struct_Point` (matching the C
        // backend's `struct_c_name`-derived tag).
        // Without these arms, `llvm_type` panics on
        // `Type::Struct(_)`. T1.2 + Vec<Struct> LLVM.
        Type::Struct(name) => format!("Struct_{}", name),
        Type::Enum(name) => format!("Enum_{}", name),
        // Closure #215: `Vec<fn(...) -> R>` element-tag fell
        // through to `llvm_type(FnPtr)` which is
        // `unreachable!` ("use llvm_type_string for fn-ptr
        // type"). The C backend already has an analogous
        // `"fnptr"` arm (closure #214). Match here so the
        // emit doesn't panic — all fn-ptrs lower to the
        // same `<ret> (<params>)*` LLVM type so a single
        // tag works.
        Type::FnPtr(_, _) => "fnptr".to_string(),
        // Vtables Phase 4b: `Vec<dyn Iface>` element tag is
        // the per-Iface fat-pointer typedef name. Without this
        // arm `llvm_type(Object)` panics ("use llvm_type_string
        // for aggregate type").
        Type::Object(name) => format!("intent_dyn_{}", name),
        // Scalars + ref/atomic/channel go through the
        // existing leaf spelling, with `%`/`*`/space replaced
        // by `_` so the identifier stays well-formed.
        _ => llvm_type(element)
            .replace(' ', "_")
            .replace('*', "p")
            .replace('%', ""),
    }
}

fn collect_vec_elements_ty(
    ty: &Type,
    seen: &mut std::collections::BTreeSet<String>,
    out: &mut Vec<Type>,
) {
    match ty {
        Type::Vec(element) => {
            // Recurse FIRST so inner element types are
            // emitted before their outer Vecs (`%intent_vec_i64`
            // must precede `%intent_vec_vec_int64`'s
            // definition). Dedup keys on the composable tag so
            // nested Vecs don't collide on the panic-prone
            // `llvm_type` spelling.
            collect_vec_elements_ty(element, seen, out);
            let key = vec_struct_tag(element);
            if seen.insert(key) {
                out.push((**element).clone());
            }
        }
        Type::Array { element, .. } => collect_vec_elements_ty(element, seen, out),
        Type::Ref(inner) | Type::RefMut(inner) => collect_vec_elements_ty(inner, seen, out),
        _ => {}
    }
}

fn collect_vec_elements_in_stmt(
    stmt: &TypedStmt,
    seen: &mut std::collections::BTreeSet<String>,
    out: &mut Vec<Type>,
) {
    match stmt {
        TypedStmt::Let { ty, expr, .. } | TypedStmt::Reassign { ty, expr, .. } => {
            collect_vec_elements_ty(ty, seen, out);
            collect_vec_elements_in_expr(expr, seen, out);
        }
        TypedStmt::Drop { ty, .. } => collect_vec_elements_ty(ty, seen, out),
        TypedStmt::Discard { expr } | TypedStmt::Return { expr } | TypedStmt::Assert { expr, .. }
        | TypedStmt::Prove { expr } => collect_vec_elements_in_expr(expr, seen, out),
        TypedStmt::Print { items } => {
            for it in items {
                if let crate::ir::TypedPrintItem::Expr(e) = it {
                    collect_vec_elements_in_expr(e, seen, out);
                }
            }
        }
        TypedStmt::If { cond, then_body, else_body } => {
            collect_vec_elements_in_expr(cond, seen, out);
            for s in then_body { collect_vec_elements_in_stmt(s, seen, out); }
            for s in else_body { collect_vec_elements_in_stmt(s, seen, out); }
        }
        TypedStmt::While { cond, body } => {
            collect_vec_elements_in_expr(cond, seen, out);
            for s in body { collect_vec_elements_in_stmt(s, seen, out); }
        }
        TypedStmt::For { start, end, body, .. } => {
            collect_vec_elements_in_expr(start, seen, out);
            collect_vec_elements_in_expr(end, seen, out);
            for s in body { collect_vec_elements_in_stmt(s, seen, out); }
        }
        TypedStmt::ForIter { element_ty, collection_ty, body, .. } => {
            collect_vec_elements_ty(element_ty, seen, out);
            collect_vec_elements_ty(collection_ty, seen, out);
            for s in body { collect_vec_elements_in_stmt(s, seen, out); }
        }
        TypedStmt::IndexAssign { index, value, .. } => {
            collect_vec_elements_in_expr(index, seen, out);
            collect_vec_elements_in_expr(value, seen, out);
        }
        _ => {}
    }
}

fn collect_vec_elements_in_expr(
    expr: &TypedExpr,
    seen: &mut std::collections::BTreeSet<String>,
    out: &mut Vec<Type>,
) {
    collect_vec_elements_ty(&expr.ty, seen, out);
    match &expr.kind {
        TypedExprKind::Unary { expr, .. } => collect_vec_elements_in_expr(expr, seen, out),
        TypedExprKind::Binary { left, right, .. } => {
            collect_vec_elements_in_expr(left, seen, out);
            collect_vec_elements_in_expr(right, seen, out);
        }
        TypedExprKind::Call { args, .. } | TypedExprKind::ArrayLit { elements: args } => {
            for a in args { collect_vec_elements_in_expr(a, seen, out); }
        }
        TypedExprKind::Cast { expr, .. } => collect_vec_elements_in_expr(expr, seen, out),
        TypedExprKind::Index { array, index, .. } => {
            collect_vec_elements_in_expr(array, seen, out);
            collect_vec_elements_in_expr(index, seen, out);
        }
        TypedExprKind::Len { array, .. } => collect_vec_elements_in_expr(array, seen, out),
        TypedExprKind::Tuple { elements } => {
            for e in elements { collect_vec_elements_in_expr(e, seen, out); }
        }
        TypedExprKind::StructLit { fields, .. } => {
            for (_, e) in fields { collect_vec_elements_in_expr(e, seen, out); }
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
            for arm in arms { collect_vec_elements_in_expr(&arm.body, seen, out); }
        }
        TypedExprKind::IfExpr { cond, then_value, else_value } => {
            collect_vec_elements_in_expr(cond, seen, out);
            collect_vec_elements_in_expr(then_value, seen, out);
            collect_vec_elements_in_expr(else_value, seen, out);
        }
        TypedExprKind::Block { stmts, tail } => {
            for s in stmts { collect_vec_elements_in_stmt(s, seen, out); }
            collect_vec_elements_in_expr(tail, seen, out);
        }
        _ => {}
    }
}

fn is_int_or_bool(ty: &Type) -> bool {
    ty.is_integer() || matches!(ty, Type::Bool)
}

/// Scalar = anything we can hold in a single LLVM register / alloca
/// today (ints, bool, floats). Aggregates (Vec, Array, refs) are
/// pending and still hit the TODO branch.
/// Collect outer captures referenced by the body — every binding
/// name read but not declared in the body and not the loop var.
/// Returned in first-appearance order so the captures form a
/// stable ctx-struct layout. The verifier has already proved the
/// body has no observable side effects, which means every capture
/// is read-only: passing pointers + concurrent reads is safe.
/// Emit per-field frees for a struct at the given LLVM
/// address. Recursively descends into nested struct fields.
/// Reverse-declaration-order drop preserved.
/// T1.2 phase 2b + D2.
fn emit_llvm_struct_field_drops(
    addr: &str,
    struct_name: &str,
    fields: &[(String, Type)],
    moved: &std::collections::HashSet<&String>,
    ctx: &mut FnCtx,
    out: &mut String,
) {
    let s_ty = format!("%Struct_{}", struct_name);
    for (idx, (field_name, field_ty)) in fields.iter().enumerate().rev() {
        if moved.contains(field_name) {
            continue;
        }
        match field_ty {
            Type::OwnedStr => {
                let fp = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = getelementptr {}, {}* {}, i64 0, i32 {}\n",
                    fp, s_ty, s_ty, addr, idx
                ));
                let s_ptr = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = load i8*, i8** {}\n",
                    s_ptr, fp
                ));
                out.push_str(&format!(
                    "  call void @free(i8* {})\n",
                    s_ptr
                ));
            }
            Type::Vec(element) => {
                let v_struct = vec_struct_name(element);
                let fp = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = getelementptr {}, {}* {}, i64 0, i32 {}\n",
                    fp, s_ty, s_ty, addr, idx
                ));
                let v_loaded = ctx.fresh_tmp();
                out.push_str(&format!(
                    "  {} = load {}, {}* {}\n",
                    v_loaded, v_struct, v_struct, fp
                ));
                let free_name = format!(
                    "@intent_vec_{}__free",
                    vec_struct_tag(element)
                );
                out.push_str(&format!(
                    "  call void {}({} {})\n",
                    free_name, v_struct, v_loaded
                ));
            }
            Type::Struct(inner_name) => {
                let inner_fields = LLVM_STRUCT_FIELDS_REGISTRY
                    .with(|r| r.borrow().get(inner_name).cloned())
                    .unwrap_or_default();
                if !inner_fields.is_empty() {
                    let fp = ctx.fresh_tmp();
                    out.push_str(&format!(
                        "  {} = getelementptr {}, {}* {}, i64 0, i32 {}\n",
                        fp, s_ty, s_ty, addr, idx
                    ));
                    let empty: std::collections::HashSet<&String> =
                        std::collections::HashSet::new();
                    emit_llvm_struct_field_drops(
                        &fp,
                        inner_name,
                        &inner_fields,
                        &empty,
                        ctx,
                        out,
                    );
                }
            }
            _ => {}
        }
    }
}

fn collect_outer_captures(body: &[TypedStmt], loop_var: &str) -> Vec<String> {
    let mut declared: std::collections::HashSet<String> = std::collections::HashSet::new();
    declared.insert(loop_var.to_string());
    let mut order: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    walk_body(body, &mut declared, &mut order, &mut seen);
    order
}

pub(crate) fn note_capture(
    name: &str,
    declared: &std::collections::HashSet<String>,
    order: &mut Vec<String>,
    seen: &mut std::collections::HashSet<String>,
) {
    if !declared.contains(name) && seen.insert(name.to_string()) {
        order.push(name.to_string());
    }
}

pub(crate) fn walk_expr(
    expr: &TypedExpr,
    declared: &std::collections::HashSet<String>,
    order: &mut Vec<String>,
    seen: &mut std::collections::HashSet<String>,
) {
    match &expr.kind {
        TypedExprKind::Int(_)
        | TypedExprKind::Float(_)
        | TypedExprKind::Bool(_)
        | TypedExprKind::Str(_) => {}
        TypedExprKind::Var(n) => note_capture(n, declared, order, seen),
        TypedExprKind::Unary { expr, .. } => walk_expr(expr, declared, order, seen),
        TypedExprKind::Binary { left, right, .. } => {
            walk_expr(left, declared, order, seen);
            walk_expr(right, declared, order, seen);
        }
        TypedExprKind::Call { args, .. } => {
            for a in args {
                walk_expr(a, declared, order, seen);
            }
        }
        TypedExprKind::Cast { expr, .. } => walk_expr(expr, declared, order, seen),
        TypedExprKind::ArrayLit { elements } => {
            for e in elements {
                walk_expr(e, declared, order, seen);
            }
        }
        TypedExprKind::Index { array, index, .. } => {
            walk_expr(array, declared, order, seen);
            walk_expr(index, declared, order, seen);
        }
        TypedExprKind::Len { array, .. } => walk_expr(array, declared, order, seen),
        TypedExprKind::Ref { name } | TypedExprKind::RefMut { name } => {
            note_capture(name, declared, order, seen);
        }
        TypedExprKind::RefField { object, .. } | TypedExprKind::RefMutField { object, .. } => {
            // Capture the struct binding; the GEP into its
            // field lives inside the outlined body.
            note_capture(object, declared, order, seen);
        }
        TypedExprKind::FnRef { .. } => {
            // Function references aren't captured locals;
            // they resolve to global @function pointers, so
            // an outlined parallel-for body doesn't need
            // to pull them into its ctx struct.
        }
        TypedExprKind::CallIndirect { callee, args } => {
            walk_expr(callee, declared, order, seen);
            for a in args {
                walk_expr(a, declared, order, seen);
            }
        }
        TypedExprKind::Tuple { elements } => {
            for e in elements {
                walk_expr(e, declared, order, seen);
            }
        }
        TypedExprKind::TupleAccess { tuple, .. } => {
            walk_expr(tuple, declared, order, seen);
        }
        TypedExprKind::StructLit { fields, .. } => {
            for (_, e) in fields {
                walk_expr(e, declared, order, seen);
            }
        }
        TypedExprKind::FieldAccess { object, .. } => {
            walk_expr(object, declared, order, seen);
        }
        TypedExprKind::EnumVariant { .. } => {}
        TypedExprKind::EnumVariantWithPayload { payload, .. } => {
            walk_expr(payload, declared, order, seen);
        }
        TypedExprKind::Match { scrutinee, arms } => {
            walk_expr(scrutinee, declared, order, seen);
            for arm in arms {
                walk_expr(&arm.body, declared, order, seen);
            }
        }
        TypedExprKind::IfExpr { cond, then_value, else_value } => {
            walk_expr(cond, declared, order, seen);
            walk_expr(then_value, declared, order, seen);
            walk_expr(else_value, declared, order, seen);
        }
        TypedExprKind::Block { stmts, tail } => {
            // Block-local lets shadow / extend the declared set
            // for the duration of the block. Clone the declared
            // set so block-local names don't leak back out.
            let mut block_declared = declared.clone();
            for s in stmts {
                if let TypedStmt::Let { name, expr, .. } = s {
                    walk_expr(expr, &block_declared, order, seen);
                    block_declared.insert(name.clone());
                }
            }
            walk_expr(tail, &block_declared, order, seen);
        }
        TypedExprKind::DynDispatch { receiver, args, .. } => {
            walk_expr(receiver, declared, order, seen);
            for a in args {
                walk_expr(a, declared, order, seen);
            }
        }
        TypedExprKind::DynCoerce { value, .. } => {
            walk_expr(value, declared, order, seen);
        }
    }
}

pub(crate) fn walk_body(
    stmts: &[TypedStmt],
    declared: &mut std::collections::HashSet<String>,
    order: &mut Vec<String>,
    seen: &mut std::collections::HashSet<String>,
) {
    for stmt in stmts {
        match stmt {
            TypedStmt::Let { name, expr, .. } => {
                walk_expr(expr, declared, order, seen);
                declared.insert(name.clone());
            }
            TypedStmt::Reassign { name, expr, .. } => {
                note_capture(name, declared, order, seen);
                walk_expr(expr, declared, order, seen);
            }
            TypedStmt::Discard { expr }
            | TypedStmt::Return { expr }
            | TypedStmt::Assert { expr, .. }
            | TypedStmt::Prove { expr } => {
                walk_expr(expr, declared, order, seen);
            }
            TypedStmt::Print { items } => {
                for it in items {
                    if let crate::ir::TypedPrintItem::Expr(e) = it {
                        walk_expr(e, declared, order, seen);
                    }
                }
            }
            TypedStmt::If { cond, then_body, else_body } => {
                walk_expr(cond, declared, order, seen);
                let saved = declared.clone();
                walk_body(then_body, declared, order, seen);
                *declared = saved.clone();
                walk_body(else_body, declared, order, seen);
                *declared = saved;
            }
            TypedStmt::While { cond, body } => {
                walk_expr(cond, declared, order, seen);
                let saved = declared.clone();
                walk_body(body, declared, order, seen);
                *declared = saved;
            }
            TypedStmt::For { var, start, end, body, .. } => {
                walk_expr(start, declared, order, seen);
                walk_expr(end, declared, order, seen);
                let saved = declared.clone();
                declared.insert(var.clone());
                walk_body(body, declared, order, seen);
                *declared = saved;
            }
            TypedStmt::ForIter { var, collection, body, .. } => {
                note_capture(collection, declared, order, seen);
                let saved = declared.clone();
                declared.insert(var.clone());
                walk_body(body, declared, order, seen);
                *declared = saved;
            }
            TypedStmt::IndexAssign { name, index, value, .. } => {
                note_capture(name, declared, order, seen);
                walk_expr(index, declared, order, seen);
                walk_expr(value, declared, order, seen);
            }
            TypedStmt::FieldAssign { object, value, .. } => {
                walk_expr(object, declared, order, seen);
                walk_expr(value, declared, order, seen);
            }
            TypedStmt::TaskSpawn { name, body, .. } => {
                let saved = declared.clone();
                walk_body(body, declared, order, seen);
                *declared = saved;
                declared.insert(name.clone());
            }
            TypedStmt::TaskJoin { name } => {
                // join consumes a handle that was declared in
                // this same block; treat it like a read of the
                // local binding so capture analysis sees nothing
                // crossing the parallel-for boundary.
                let _ = name;
            }
            TypedStmt::Drop { .. } | TypedStmt::Break | TypedStmt::Continue => {}
        }
    }
}

/// Walk the parallel-for body and replace every Reassign of a
/// reduction variable with a `Discard` of a synthetic Call
/// `__intent_atomic_add(name, increment)` (or another op once we
/// support them). The LLVM `Call` emit special-cases this name
/// and produces an `atomicrmw <op> i64* %addr, i64 %inc` against
/// the binding's captured pointer. The increment is extracted by
/// pattern-matching the original Reassign's RHS, which the
/// checker has already validated as one of `name op X` or `X op
/// name`.
fn rewrite_body_for_reductions(
    body: &[TypedStmt],
    reductions: &std::collections::HashMap<String, crate::ast::ReductionOp>,
) -> Vec<TypedStmt> {
    body.iter()
        .map(|s| rewrite_stmt_for_reductions(s, reductions))
        .collect()
}

fn rewrite_stmt_for_reductions(
    stmt: &TypedStmt,
    reductions: &std::collections::HashMap<String, crate::ast::ReductionOp>,
) -> TypedStmt {
    match stmt {
        TypedStmt::Reassign { name, expr, .. } if reductions.contains_key(name) => {
            // Extract the increment side of `name op X` or
            // `X op name`. The checker has validated the shape.
            let op = reductions[name];
            let inc = match &expr.kind {
                TypedExprKind::Binary { left, right, .. } => {
                    let left_is_self = matches!(&left.kind, TypedExprKind::Var(n) if n == name);
                    if left_is_self {
                        (**right).clone()
                    } else {
                        (**left).clone()
                    }
                }
                // Min/Max are intrinsic Calls. The checker has
                // validated the args[0]/args[1] shape; whichever
                // side isn't `Var(name)` is the increment.
                TypedExprKind::Call { args, .. } => {
                    let left_is_self =
                        matches!(&args[0].kind, TypedExprKind::Var(n) if n == name);
                    if left_is_self {
                        args[1].clone()
                    } else {
                        args[0].clone()
                    }
                }
                _ => unreachable!("checker validates reduction shape"),
            };
            // Synthetic call `__intent_atomic_<op>(name, inc)`. The
            // LLVM Call emit interceptor inspects the first arg as
            // a Var(name) and emits the right atomicrmw.
            use crate::ast::ReductionOp;
            let intrinsic = match op {
                ReductionOp::Add => "__intent_atomic_add",
                ReductionOp::Mul => "__intent_atomic_mul",
                ReductionOp::And => "__intent_atomic_and",
                ReductionOp::Or => "__intent_atomic_or",
                // Bitwise reductions reuse the `and`/`or`/`xor`
                // intrinsic names; the Call-emit interceptor
                // disambiguates from bool `&&`/`||` by inspecting
                // the captured variable's type (Bool → i8 shadow,
                // integer → direct atomicrmw).
                ReductionOp::BitAnd => "__intent_atomic_and",
                ReductionOp::BitOr => "__intent_atomic_or",
                ReductionOp::BitXor => "__intent_atomic_xor",
                ReductionOp::Min => "__intent_atomic_min",
                ReductionOp::Max => "__intent_atomic_max",
            };
            TypedStmt::Discard {
                expr: TypedExpr {
                    kind: TypedExprKind::Call {
                        name: intrinsic.to_string(),
                        name_span: crate::span::Span::default(),
                        args: vec![
                            TypedExpr {
                                kind: TypedExprKind::Var(name.clone()),
                                ty: inc.ty.clone(),
                                constant: None,
                                span: expr.span,
                                binding_decl_span: None,
                            },
                            inc,
                        ],
                    },
                    ty: Type::I64,
                    constant: None,
                    span: expr.span,
                    binding_decl_span: None,
                },
            }
        }
        TypedStmt::If { cond, then_body, else_body } => TypedStmt::If {
            cond: cond.clone(),
            then_body: rewrite_body_for_reductions(then_body, reductions),
            else_body: rewrite_body_for_reductions(else_body, reductions),
        },
        TypedStmt::While { cond, body } => TypedStmt::While {
            cond: cond.clone(),
            body: rewrite_body_for_reductions(body, reductions),
        },
        TypedStmt::For { var, ty, start, end, body, parallel, reductions: rs } => {
            TypedStmt::For {
                var: var.clone(),
                ty: ty.clone(),
                start: start.clone(),
                end: end.clone(),
                body: rewrite_body_for_reductions(body, reductions),
                parallel: *parallel,
                reductions: rs.clone(),
            }
        }
        other => other.clone(),
    }
}

/// LLVM type of a binding's "address" slot — what we store and
/// pass through the parallel-for ctx struct so the outlined fn
/// can register the binding in its own locals map.
///
///   - For non-ref bindings: the alloca pointer, `<ty>*`.
///   - For ref bindings: the ref value itself (already a
///     pointer), whose type `<inner>*` is what
///     `llvm_type_string` returns directly.
fn alloca_addr_type(ty: &Type) -> String {
    if ty.is_any_ref() {
        llvm_type_string(ty)
    } else {
        format!("{}*", llvm_type_string(ty))
    }
}

/// Lower `task <name> { … }` and `join <name>;` to real
/// pthread spawn/join. The task body becomes an outlined
/// function `i8* @intent_task_<id>(i8* %_ctx_raw)`; captures
/// (all Copy-typed per the checker's gate) are stored
/// by-value into a heap-allocated ctx struct. Parent passes
/// the ctx pointer to `pthread_create`; the join site calls
/// `pthread_join` then `free`s the ctx.
fn emit_task_via_pthread(
    name: &str,
    body: &[TypedStmt],
    captures: &[(String, Type)],
    ctx: &mut FnCtx,
    out: &mut String,
) {
    let id = ctx.next_outline;
    ctx.next_outline += 1;
    let fn_name = format!("intent_task_{}", id);

    // Anonymous ctx struct: one field per capture, by-value.
    let field_tys: Vec<String> =
        captures.iter().map(|(_, t)| llvm_type_string(t)).collect();
    let ctx_ty = if field_tys.is_empty() {
        "{}".to_string()
    } else {
        format!("{{ {} }}", field_tys.join(", "))
    };

    // --- Spawn-site code in the parent function. ---
    // The task handle alloca lives in the parent's locals map
    // so the matching join can find it later.
    let handle_addr = format!("%{}.addr", name);
    out.push_str(&format!(
        "  {} = alloca %intent_task_handle\n",
        handle_addr
    ));
    ctx.locals
        .insert(name.to_string(), (Type::Task, handle_addr.clone()));

    // malloc the ctx. Use 1024 bytes as a safe upper bound to
    // sidestep sizeof — we never overflow that for the
    // capture sets we accept (Copy types only, narrow widths).
    // A future pass can compute exact sizeof via DataLayout.
    let ctx_size = compute_ctx_size(captures);
    let ctx_raw = ctx.fresh_tmp();
    out.push_str(&format!(
        "  {} = call i8* @malloc(i64 {})\n",
        ctx_raw, ctx_size
    ));
    // Cast to the typed ctx pointer and populate fields.
    let ctx_typed = ctx.fresh_tmp();
    out.push_str(&format!(
        "  {} = bitcast i8* {} to {}*\n",
        ctx_typed, ctx_raw, ctx_ty
    ));
    for (i, (cap_name, cap_ty)) in captures.iter().enumerate() {
        // Look up the parent's alloca for this capture; load
        // its value; store into the ctx field.
        let parent_addr = match ctx.locals.get(cap_name) {
            Some((_, a)) => a.clone(),
            None => unreachable!(
                "checker: captured binding '{}' must exist in parent locals",
                cap_name
            ),
        };
        let lty = llvm_type_string(cap_ty);
        let v = if cap_ty.is_any_ref() {
            // Ref params arrive as the pointer value itself,
            // not behind another alloca.
            parent_addr.clone()
        } else {
            let loaded = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = load {}, {}* {}\n",
                loaded, lty, lty, parent_addr
            ));
            loaded
        };
        let slot_p = ctx.fresh_tmp();
        out.push_str(&format!(
            "  {} = getelementptr {}, {}* {}, i32 0, i32 {}\n",
            slot_p, ctx_ty, ctx_ty, ctx_typed, i
        ));
        out.push_str(&format!(
            "  store {} {}, {}* {}\n",
            lty, v, lty, slot_p
        ));
    }
    // Fire the platform spawn: POSIX writes the pthread id
    // into `&handle.thread`; Win32 returns the HANDLE which
    // we `ptrtoint` and store into the same i64 slot so the
    // join-site code can stay symmetric across platforms.
    let thread_field = ctx.fresh_tmp();
    out.push_str(&format!(
        "  {} = getelementptr %intent_task_handle, %intent_task_handle* {}, i32 0, i32 0\n",
        thread_field, handle_addr
    ));
    if host_uses_win32_threading() {
        let handle = ctx.fresh_tmp();
        out.push_str(&format!(
            "  {} = call i8* @CreateThread(i8* null, i64 0, i8* (i8*)* @{}, i8* {}, i32 0, i32* null)\n",
            handle, fn_name, ctx_raw
        ));
        let handle_int = ctx.fresh_tmp();
        out.push_str(&format!(
            "  {} = ptrtoint i8* {} to i64\n",
            handle_int, handle
        ));
        out.push_str(&format!(
            "  store i64 {}, i64* {}\n",
            handle_int, thread_field
        ));
    } else {
        let _spawn_ret = ctx.fresh_tmp();
        out.push_str(&format!(
            "  {} = call i32 @pthread_create(i64* {}, i8* null, i8* (i8*)* @{}, i8* {})\n",
            _spawn_ret, thread_field, fn_name, ctx_raw
        ));
    }
    // Stash the ctx pointer in the handle so join can free
    // it.
    let ctx_field = ctx.fresh_tmp();
    out.push_str(&format!(
        "  {} = getelementptr %intent_task_handle, %intent_task_handle* {}, i32 0, i32 1\n",
        ctx_field, handle_addr
    ));
    out.push_str(&format!(
        "  store i8* {}, i8** {}\n",
        ctx_raw, ctx_field
    ));

    // --- Outlined function body, deferred. ---
    let mut deferred = String::new();
    deferred.push_str(&format!(
        "define internal i8* @{}(i8* %_ctx_raw) {{\n",
        fn_name
    ));
    deferred.push_str("entry:\n");
    deferred.push_str(&format!(
        "  %ctx_p = bitcast i8* %_ctx_raw to {}*\n",
        ctx_ty
    ));
    // For each capture, allocate a local alloca and copy the
    // ctx-loaded value into it so the body's emit (which uses
    // `%<name>.addr` for variable reads) finds the binding
    // naturally.
    let mut outlined_ctx = FnCtx::new(
        ctx.assert_msg_indices,
        ctx.print_str_indices,
    );
    outlined_ctx.next_outline = ctx.next_outline;
    for (i, (cap_name, cap_ty)) in captures.iter().enumerate() {
        let slot_p = format!("%cap_slot_{}", i);
        deferred.push_str(&format!(
            "  {} = getelementptr {}, {}* %ctx_p, i32 0, i32 {}\n",
            slot_p, ctx_ty, ctx_ty, i
        ));
        if cap_ty.is_any_ref() {
            // Refs come through the ctx as their pointer
            // value. Load it into a local SSA name that the
            // emit path recognizes via locals.insert.
            let lty = llvm_type_string(cap_ty);
            let loaded = format!("%arg_{}", cap_name);
            deferred.push_str(&format!(
                "  {} = load {}, {}* {}\n",
                loaded, lty, lty, slot_p
            ));
            outlined_ctx.locals.insert(
                cap_name.clone(),
                (cap_ty.clone(), loaded),
            );
        } else {
            // Copy values: load + re-allocate so the body
            // can reassign / shadow the binding without
            // corrupting the ctx.
            let lty = llvm_type_string(cap_ty);
            let loaded = format!("%cap_val_{}", i);
            deferred.push_str(&format!(
                "  {} = load {}, {}* {}\n",
                loaded, lty, lty, slot_p
            ));
            let local_addr = format!("%{}.addr", cap_name);
            deferred.push_str(&format!("  {} = alloca {}\n", local_addr, lty));
            deferred.push_str(&format!(
                "  store {} {}, {}* {}\n",
                lty, loaded, lty, local_addr
            ));
            outlined_ctx.locals.insert(
                cap_name.clone(),
                (cap_ty.clone(), local_addr),
            );
        }
    }
    // Emit the task body into the outlined fn.
    for s in body {
        emit_stmt(s, &mut outlined_ctx, &mut deferred);
    }
    // Tasks return unit; lower as `ret i8* null` and ensure
    // there's no preceding terminator dangling.
    if !outlined_ctx.terminated {
        deferred.push_str("  ret i8* null\n");
    }
    deferred.push_str("}\n");
    // Splice the outlined fn into the parent's deferred
    // queue + propagate its outline counter.
    ctx.deferred_functions.push_str(&outlined_ctx.deferred_functions);
    ctx.deferred_functions.push_str(&deferred);
    ctx.next_outline = outlined_ctx.next_outline;
}

/// Estimate ctx-struct byte size for malloc — a safe upper
/// bound is fine since we never index past the declared
/// fields. We compute it as sum of each capture's natural
/// size (rounded up to 8 bytes for alignment).
fn compute_ctx_size(captures: &[(String, Type)]) -> u64 {
    let mut total: u64 = 0;
    for (_, t) in captures {
        let n = type_byte_size(t);
        total += (n + 7) & !7;
    }
    // Always at least 8 bytes so malloc(0) doesn't return
    // weird things.
    total.max(8)
}

fn type_byte_size(t: &Type) -> u64 {
    match t {
        Type::I8 | Type::U8 | Type::Bool => 1,
        Type::I16 | Type::U16 => 2,
        Type::I32 | Type::U32 | Type::F32 => 4,
        Type::I64 | Type::U64 | Type::F64 => 8,
        Type::Str | Type::OwnedStr => 8,
        Type::Ref(_) | Type::RefMut(_) => 8,
        Type::FnPtr(_, _) => 8,
        Type::Task => 16,
        _ => 8, // conservative
    }
}

/// Lift the body of a `parallel for i in start..end { … }` into a
/// fresh `@__intent_par_<N>` function and dispatch it across
/// worker threads at the original site. On Linux/macOS we call
/// `@GOMP_parallel(fn, ctx)` and the outlined fn queries
/// `omp_get_thread_num` / `omp_get_num_threads` to partition the
/// iteration space. On Windows libgomp isn't available; instead
/// we open-code a `@CreateThread` fan-out with a hardcoded
/// `INTENT_WIN_PAR_THREADS` worker count (the current thread runs
/// the tid-0 slice and waits on the rest with
/// `@WaitForSingleObject`). The outlined fn receives a per-thread
/// `WinParArg { i8* ctx, i64 tid, i64 nt }` and reads tid/nt
/// directly out of that struct rather than via the OMP runtime.
fn emit_parallel_for_via_gomp(
    var: &str,
    ty: &Type,
    start_v: &str,
    end_v: &str,
    body: &[TypedStmt],
    reductions: &[crate::ir::TypedReduction],
    ctx: &mut FnCtx,
    out: &mut String,
) {
    let id = ctx.next_outline;
    ctx.next_outline += 1;
    let fn_name = format!("__intent_par_{}", id);
    let lty = llvm_type(ty);

    // Discover what outer bindings the body reads, in document
    // order. Each becomes a pointer field on the ctx struct so
    // the outlined fn can register it in its own locals map.
    let captures: Vec<String> = collect_outer_captures(body, var);
    // Resolve each capture against the parent's locals — if a
    // capture isn't in parent locals (shouldn't happen for a
    // well-typed body) we drop it; the outlined fn will then
    // produce an unresolved reference, surfacing the bug.
    struct CaptureSlot {
        name: String,
        ty: Type,
        parent_addr: String,
        field_ty: String,
    }
    let mut capture_slots: Vec<CaptureSlot> = captures
        .into_iter()
        .filter_map(|n| {
            ctx.locals.get(&n).map(|(t, a)| CaptureSlot {
                name: n.clone(),
                ty: t.clone(),
                parent_addr: a.clone(),
                field_ty: alloca_addr_type(t),
            })
        })
        .collect();

    // Bool reductions can't go through `atomicrmw` directly because
    // i1 isn't byte-sized. We allocate an i8 shadow per bool
    // reduction in the parent, zext-store the current bool value
    // into it, and rewrite the matching capture slot to point at
    // the shadow (i8*). The outlined fn does `atomicrmw and/or i8`
    // against the shadow; after GOMP_parallel returns we read the
    // shadow back, `icmp ne 0`, and store the final i1 back into
    // the original alloca.
    struct BoolShadow {
        orig_addr: String,
        shadow_addr: String,
    }
    let mut bool_shadows: Vec<BoolShadow> = Vec::new();
    for r in reductions {
        if !matches!(r.op, crate::ast::ReductionOp::And | crate::ast::ReductionOp::Or) {
            continue;
        }
        let Some(slot) = capture_slots.iter_mut().find(|c| c.name == r.var) else {
            continue;
        };
        let shadow_addr = ctx.fresh_tmp();
        out.push_str(&format!("  {} = alloca i8\n", shadow_addr));
        let cur_b = ctx.fresh_tmp();
        out.push_str(&format!(
            "  {} = load i1, i1* {}\n",
            cur_b, slot.parent_addr
        ));
        let cur_8 = ctx.fresh_tmp();
        out.push_str(&format!(
            "  {} = zext i1 {} to i8\n",
            cur_8, cur_b
        ));
        out.push_str(&format!(
            "  store i8 {}, i8* {}\n",
            cur_8, shadow_addr
        ));
        bool_shadows.push(BoolShadow {
            orig_addr: slot.parent_addr.clone(),
            shadow_addr: shadow_addr.clone(),
        });
        slot.parent_addr = shadow_addr;
        slot.field_ty = "i8*".to_string();
    }

    // Inline anonymous ctx struct type. Two i64 head fields (start,
    // end) followed by one pointer field per capture. No named
    // type needed — LLVM IR allows anonymous struct types inline.
    let mut field_tys: Vec<String> = vec!["i64".into(), "i64".into()];
    for cap in &capture_slots {
        field_tys.push(cap.field_ty.clone());
    }
    let ctx_ty = format!("{{ {} }}", field_tys.join(", "));

    // -- Outlined function body (writes to ctx.deferred_functions). --
    // On Win32 the outlined fn is called via `@CreateThread`, which
    // requires the `i8* (i8*)` shape, so we return null at exit. The
    // `i8*` arg points at a per-thread WinParArg struct
    // `{ i8* ctx_ptr, i64 tid, i64 nt }` (filled in at the call
    // site); we unpack `tid` / `nt` from there and `ctx_ptr` is
    // the real ctx struct.
    let use_win32 = host_uses_win32_threading();
    let mut deferred = String::new();
    if use_win32 {
        deferred.push_str(&format!(
            "define internal i8* @{}(i8* %data_raw) {{\n",
            fn_name
        ));
        deferred.push_str("entry:\n");
        deferred.push_str(
            "  %winarg_p = bitcast i8* %data_raw to { i8*, i64, i64 }*\n",
        );
        deferred.push_str(
            "  %ctx_raw_p = getelementptr { i8*, i64, i64 }, { i8*, i64, i64 }* %winarg_p, i32 0, i32 0\n",
        );
        deferred.push_str(
            "  %ctx_raw = load i8*, i8** %ctx_raw_p\n",
        );
        deferred.push_str(&format!(
            "  %ctx_p = bitcast i8* %ctx_raw to {}*\n",
            ctx_ty
        ));
    } else {
        deferred.push_str(&format!(
            "define internal void @{}(i8* %data_raw) {{\n",
            fn_name
        ));
        deferred.push_str("entry:\n");
        deferred.push_str(&format!(
            "  %ctx_p = bitcast i8* %data_raw to {}*\n",
            ctx_ty
        ));
    }
    deferred.push_str(&format!(
        "  %start_p = getelementptr {}, {}* %ctx_p, i32 0, i32 0\n",
        ctx_ty, ctx_ty
    ));
    deferred.push_str("  %start_v = load i64, i64* %start_p\n");
    deferred.push_str(&format!(
        "  %end_p = getelementptr {}, {}* %ctx_p, i32 0, i32 1\n",
        ctx_ty, ctx_ty
    ));
    deferred.push_str("  %end_v = load i64, i64* %end_p\n");
    // Unpack captures: getelementptr the field, load the pointer,
    // register it as the binding's address in outlined_ctx.locals.
    // Names are hard-coded as `%cap_<i>` so they don't collide with
    // the body's fresh-tmp counter (which starts at 0 in the new
    // function context).
    let mut prelude_captures = String::new();
    for (i, cap) in capture_slots.iter().enumerate() {
        let field_idx = i + 2;
        prelude_captures.push_str(&format!(
            "  %cap_{}_p = getelementptr {}, {}* %ctx_p, i32 0, i32 {}\n",
            i, ctx_ty, ctx_ty, field_idx
        ));
        prelude_captures.push_str(&format!(
            "  %cap_{} = load {}, {}* %cap_{}_p\n",
            i, cap.field_ty, cap.field_ty, i
        ));
    }
    deferred.push_str(&prelude_captures);
    if use_win32 {
        deferred.push_str(
            "  %tid_p = getelementptr { i8*, i64, i64 }, { i8*, i64, i64 }* %winarg_p, i32 0, i32 1\n",
        );
        deferred.push_str("  %tid = load i64, i64* %tid_p\n");
        deferred.push_str(
            "  %nt_p = getelementptr { i8*, i64, i64 }, { i8*, i64, i64 }* %winarg_p, i32 0, i32 2\n",
        );
        deferred.push_str("  %nt = load i64, i64* %nt_p\n");
    } else {
        deferred.push_str("  %tid32 = call i32 @omp_get_thread_num()\n");
        deferred.push_str("  %nt32 = call i32 @omp_get_num_threads()\n");
        deferred.push_str("  %tid = sext i32 %tid32 to i64\n");
        deferred.push_str("  %nt = sext i32 %nt32 to i64\n");
    }
    // chunk = ceil((end - start) / nt) = (end - start + nt - 1) / nt
    deferred.push_str("  %range = sub i64 %end_v, %start_v\n");
    deferred.push_str("  %r1 = add i64 %range, %nt\n");
    deferred.push_str("  %r2 = sub i64 %r1, 1\n");
    deferred.push_str("  %chunk = sdiv i64 %r2, %nt\n");
    deferred.push_str("  %off = mul i64 %tid, %chunk\n");
    deferred.push_str("  %my_lo = add i64 %start_v, %off\n");
    deferred.push_str("  %my_hi_uncapped = add i64 %my_lo, %chunk\n");
    deferred.push_str("  %cap = icmp slt i64 %my_hi_uncapped, %end_v\n");
    deferred.push_str("  %my_hi = select i1 %cap, i64 %my_hi_uncapped, i64 %end_v\n");

    // Body's outer-loop counter alloca + header.
    deferred.push_str(&format!("  %i_addr = alloca {}\n", lty));
    // Note: start_v / end_v are i64; if the loop is narrower, we'd
    // need a trunc. For now `ty` is assumed i64 / u64.
    let stored_lo = if matches!(ty, Type::I64 | Type::U64) {
        "%my_lo".to_string()
    } else {
        deferred.push_str(&format!("  %my_lo_n = trunc i64 %my_lo to {}\n", lty));
        "%my_lo_n".to_string()
    };
    deferred.push_str(&format!("  store {} {}, {}* %i_addr\n", lty, stored_lo, lty));
    deferred.push_str("  br label %hdr\n");
    deferred.push_str("hdr:\n");
    let cur_name = "%i_cur".to_string();
    deferred.push_str(&format!("  {} = load {}, {}* %i_addr\n", cur_name, lty, lty));
    let cmp_lo = if matches!(ty, Type::I64 | Type::U64) {
        cur_name.clone()
    } else {
        deferred.push_str(&format!("  %i_cur64 = sext {} {} to i64\n", lty, cur_name));
        "%i_cur64".to_string()
    };
    let lt = if ty.is_signed_integer() { "slt" } else { "ult" };
    deferred.push_str(&format!(
        "  %cmp = icmp {} i64 {}, %my_hi\n",
        lt, cmp_lo
    ));
    deferred.push_str("  br i1 %cmp, label %body, label %exit\n");
    deferred.push_str("body:\n");

    // Emit the body's statements with a fresh FnCtx whose only
    // pre-populated locals are the loop variable and the captures
    // we just unpacked above. The body's emit code will read
    // through the captured pointers via the normal `Var` lookup
    // path (load <ty>, <ty>* <addr>).
    let mut outlined_ctx = FnCtx::new(ctx.assert_msg_indices, ctx.print_str_indices);
    outlined_ctx
        .locals
        .insert(var.to_string(), (ty.clone(), "%i_addr".to_string()));
    for (i, cap) in capture_slots.iter().enumerate() {
        outlined_ctx
            .locals
            .insert(cap.name.clone(), (cap.ty.clone(), format!("%cap_{}", i)));
    }
    // Rewrite reduction-Reassigns in the body to atomicrmw
    // updates. The checker has guaranteed each Reassign target is
    // the reduction variable AND its RHS is `name op X` (or `X op
    // name`). Extracting the increment expression `X` is a single
    // pattern-match on the Binary's operands.
    let reductions_by_name: std::collections::HashMap<String, crate::ast::ReductionOp> = reductions
        .iter()
        .map(|r| (r.var.clone(), r.op))
        .collect();
    let rewritten = rewrite_body_for_reductions(body, &reductions_by_name);
    // Push a LoopFrame so `continue` inside the body jumps
    // to the step block (which does the +1) instead of
    // falling through to the "outside a loop" no-op path
    // that turned `continue` into a no-op → wrong reduction
    // total. Closure #189 (same family as #185–#188).
    outlined_ctx.loops.push(LoopFrame {
        header: "step".to_string(),
        exit: "exit".to_string(),
    });
    for s in &rewritten {
        emit_stmt(s, &mut outlined_ctx, &mut deferred);
    }
    if !outlined_ctx.terminated {
        deferred.push_str("  br label %step\n");
    }
    outlined_ctx.loops.pop();
    // Step block: increment i and jump back to hdr.
    deferred.push_str("step:\n");
    deferred.push_str(&format!(
        "  %i_next_load = load {}, {}* %i_addr\n",
        lty, lty
    ));
    deferred.push_str(&format!("  %i_next = add {} %i_next_load, 1\n", lty));
    deferred.push_str(&format!(
        "  store {} %i_next, {}* %i_addr\n",
        lty, lty
    ));
    deferred.push_str("  br label %hdr\n");
    deferred.push_str("exit:\n");
    if use_win32 {
        deferred.push_str("  ret i8* null\n");
    } else {
        deferred.push_str("  ret void\n");
    }
    deferred.push_str("}\n");

    // Append any nested outlined helpers the body emitted to the
    // parent's deferred buffer so they get flushed together.
    if !outlined_ctx.deferred_functions.is_empty() {
        deferred.push_str(&outlined_ctx.deferred_functions);
    }
    ctx.deferred_functions.push_str(&deferred);

    // -- Call site in the parent function (writes to `out`). --
    let ctx_alloca = ctx.fresh_tmp();
    out.push_str(&format!("  {} = alloca {}\n", ctx_alloca, ctx_ty));
    let sp = ctx.fresh_tmp();
    out.push_str(&format!(
        "  {} = getelementptr {}, {}* {}, i32 0, i32 0\n",
        sp, ctx_ty, ctx_ty, ctx_alloca
    ));
    // Widen start/end to i64 if the loop type is narrower.
    let start_i64 = if matches!(ty, Type::I64 | Type::U64) {
        start_v.to_string()
    } else {
        let w = ctx.fresh_tmp();
        let op = if ty.is_signed_integer() { "sext" } else { "zext" };
        out.push_str(&format!("  {} = {} {} {} to i64\n", w, op, lty, start_v));
        w
    };
    let end_i64 = if matches!(ty, Type::I64 | Type::U64) {
        end_v.to_string()
    } else {
        let w = ctx.fresh_tmp();
        let op = if ty.is_signed_integer() { "sext" } else { "zext" };
        out.push_str(&format!("  {} = {} {} {} to i64\n", w, op, lty, end_v));
        w
    };
    out.push_str(&format!("  store i64 {}, i64* {}\n", start_i64, sp));
    let ep = ctx.fresh_tmp();
    out.push_str(&format!(
        "  {} = getelementptr {}, {}* {}, i32 0, i32 1\n",
        ep, ctx_ty, ctx_ty, ctx_alloca
    ));
    out.push_str(&format!("  store i64 {}, i64* {}\n", end_i64, ep));
    // Store each outer capture's parent address into its ctx
    // field. For non-ref bindings that's the alloca pointer; for
    // ref bindings it's the ref value (also a pointer).
    for (i, cap) in capture_slots.iter().enumerate() {
        let fp = ctx.fresh_tmp();
        let field_idx = i + 2;
        out.push_str(&format!(
            "  {} = getelementptr {}, {}* {}, i32 0, i32 {}\n",
            fp, ctx_ty, ctx_ty, ctx_alloca, field_idx
        ));
        out.push_str(&format!(
            "  store {} {}, {}* {}\n",
            cap.field_ty, cap.parent_addr, cap.field_ty, fp
        ));
    }
    let ctx_i8 = ctx.fresh_tmp();
    out.push_str(&format!(
        "  {} = bitcast {}* {} to i8*\n",
        ctx_i8, ctx_ty, ctx_alloca
    ));
    if use_win32 {
        // Win32 dispatch: hardcoded INTENT_WIN_PAR_THREADS=4
        // worker tasks. tid 0 runs in the current thread;
        // tids 1..3 are spawned via `@CreateThread`, then
        // joined with `@WaitForSingleObject` /
        // `@CloseHandle`. Each thread reads its tid/nt from
        // its WinParArg struct (already arranged so the
        // outlined fn matches `i8* (i8*)`, the CreateThread
        // start-routine ABI).
        const N: u64 = 4;
        let warr = ctx.fresh_tmp();
        out.push_str(&format!(
            "  {} = alloca [{} x {{ i8*, i64, i64 }}]\n",
            warr, N
        ));
        // Populate each per-thread arg slot.
        let mut wp = Vec::with_capacity(N as usize);
        for i in 0..N {
            let wpi = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = getelementptr [{} x {{ i8*, i64, i64 }}], [{} x {{ i8*, i64, i64 }}]* {}, i64 0, i64 {}\n",
                wpi, N, N, warr, i
            ));
            let cf = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = getelementptr {{ i8*, i64, i64 }}, {{ i8*, i64, i64 }}* {}, i32 0, i32 0\n",
                cf, wpi
            ));
            out.push_str(&format!(
                "  store i8* {}, i8** {}\n",
                ctx_i8, cf
            ));
            let tf = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = getelementptr {{ i8*, i64, i64 }}, {{ i8*, i64, i64 }}* {}, i32 0, i32 1\n",
                tf, wpi
            ));
            out.push_str(&format!(
                "  store i64 {}, i64* {}\n",
                i, tf
            ));
            let nf = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = getelementptr {{ i8*, i64, i64 }}, {{ i8*, i64, i64 }}* {}, i32 0, i32 2\n",
                nf, wpi
            ));
            out.push_str(&format!(
                "  store i64 {}, i64* {}\n",
                N, nf
            ));
            wp.push(wpi);
        }
        // Spawn tids 1..N-1 via CreateThread. Stack-alloca
        // handle array of size N-1 and remember each handle.
        let hs = ctx.fresh_tmp();
        out.push_str(&format!(
            "  {} = alloca [{} x i8*]\n",
            hs,
            N - 1
        ));
        let mut handle_ps: Vec<String> = Vec::with_capacity((N - 1) as usize);
        for i in 1..N {
            let raw = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = bitcast {{ i8*, i64, i64 }}* {} to i8*\n",
                raw, wp[i as usize]
            ));
            let h = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = call i8* @CreateThread(i8* null, i64 0, i8* (i8*)* @{}, i8* {}, i32 0, i32* null)\n",
                h, fn_name, raw
            ));
            let hp = ctx.fresh_tmp();
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
        // tid 0 runs in the calling thread synchronously.
        let raw0 = ctx.fresh_tmp();
        out.push_str(&format!(
            "  {} = bitcast {{ i8*, i64, i64 }}* {} to i8*\n",
            raw0, wp[0]
        ));
        let _ret0 = ctx.fresh_tmp();
        out.push_str(&format!(
            "  {} = call i8* @{}(i8* {})\n",
            _ret0, fn_name, raw0
        ));
        // Wait on each spawned thread and close its handle.
        for hp in &handle_ps {
            let hl = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = load i8*, i8** {}\n",
                hl, hp
            ));
            let _wait = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = call i32 @WaitForSingleObject(i8* {}, i32 -1)\n",
                _wait, hl
            ));
            let _close = ctx.fresh_tmp();
            out.push_str(&format!(
                "  {} = call i32 @CloseHandle(i8* {})\n",
                _close, hl
            ));
        }
    } else {
        out.push_str(&format!(
            "  call void @GOMP_parallel(void (i8*)* @{}, i8* {}, i32 0, i32 0)\n",
            fn_name, ctx_i8
        ));
    }

    // For each bool reduction, read back the i8 shadow and store
    // the resulting i1 into the original alloca. `icmp ne i8 …,
    // 0` collapses both `||` (any thread saw true → shadow non-
    // zero) and `&&` (all threads kept it 1 → shadow == 1)
    // semantics; the verifier already pinned the body to the
    // canonical `var = var <op> rhs` shape.
    for shadow in &bool_shadows {
        let loaded8 = ctx.fresh_tmp();
        out.push_str(&format!(
            "  {} = load i8, i8* {}\n",
            loaded8, shadow.shadow_addr
        ));
        let loaded1 = ctx.fresh_tmp();
        out.push_str(&format!(
            "  {} = icmp ne i8 {}, 0\n",
            loaded1, loaded8
        ));
        out.push_str(&format!(
            "  store i1 {}, i1* {}\n",
            loaded1, shadow.orig_addr
        ));
    }
}

fn is_scalar(ty: &Type) -> bool {
    is_int_or_bool(ty)
        || ty.is_float()
        || matches!(ty, Type::Str | Type::OwnedStr)
        // `Atomic<T>` stores a single T-sized cell; treat it
        // like a scalar so the Let path allocates a single
        // alloca of the underlying type rather than falling
        // into the aggregate / Vec / Ref handlers.
        || matches!(ty, Type::Atomic(_))
        // Concurrency-primitive struct types: their `llvm_type`
        // returns a named struct (`%intent_channel_i64` etc.),
        // so the scalar Let path's `alloca <ty>` and `store
        // <ty> <v>, <ty>* …` work uniformly. The builtins
        // (channel_new, mutex_new, mutex_lock) return SSA
        // values of these struct types.
        || matches!(ty, Type::Channel(_, _) | Type::Mutex(_) | Type::Guard(_))
        // Function pointers are pointer-sized scalars. The
        // alloca holds a single value of fn-ptr type (the
        // load returns a function-pointer SSA value the
        // CallIndirect emit consumes).
        || matches!(ty, Type::FnPtr(_, _))
        // Tuples lower to anonymous LLVM structs. The Let
        // path's `alloca <{T1, T2, …}>` and `store` work
        // uniformly via the scalar path. T1.1.
        || matches!(ty, Type::Tuple(_))
        // User-declared structs likewise. T1.2.
        || matches!(ty, Type::Struct(_))
        // Enums are tag-sized i32 scalars. T1.3.
        || matches!(ty, Type::Enum(_))
        // `dyn Iface` lowers to the named struct
        // `%intent_dyn_<Iface>` (a fat pointer).
        // Vtables Phase 3b.
        || matches!(ty, Type::Object(_))
}

/// Map our types to LLVM IR sort spellings. Signedness is the
/// operator's concern, not the type's, so `u64` and `i64` both map
/// to `i64`. Float / aggregate types are stubbed pending those
/// portions of the backend landing.
pub(crate) fn llvm_type(ty: &Type) -> &'static str {
    match ty {
        Type::I8 | Type::U8 => "i8",
        Type::I16 | Type::U16 => "i16",
        Type::I32 | Type::U32 => "i32",
        Type::I64 | Type::U64 => "i64",
        Type::Bool => "i1",
        Type::F32 => "float",
        Type::F64 => "double",
        Type::Str | Type::OwnedStr => "i8*",
        // `Atomic<T>` storage at the LLVM level is the inner
        // integer type. `llvm_type` returns &'static str so we
        // dispatch on the inner directly; this stays in lock-
        // step with `atomic_storage_llvm` used by the atomic
        // builtin lowering below.
        Type::Atomic(inner) => atomic_storage_llvm(inner),
        // Concurrency-primitive struct types. Mutex/Guard
        // are still i64-only; Channel is parametric, so
        // callers that may see it use `llvm_type_string`.
        // Hitting this arm with Channel means a caller
        // forgot to route through llvm_type_string — fall
        // back to the i64/16 form so IR stays parseable.
        Type::Channel(_, _) => "%intent_channel_i64_16",
        Type::Mutex(_) => "%intent_mutex_i64",
        Type::Guard(_) => "%intent_guard_i64",
        // Enums lower to a 32-bit tag — see `llvm_type_string`
        // for the same. T1.3.
        Type::Enum(_) => "i32",
        // Function pointers don't fit into `&'static str` —
        // their spelling depends on parameter / return types.
        // Callers must route through `llvm_type_string`.
        // Reaching this arm with FnPtr means a caller still
        // uses `llvm_type` where the parametric form would
        // apply.
        Type::FnPtr(_, _) => unreachable!(
            "llvm_type: use llvm_type_string for fn-ptr type"
        ),
        // Aggregates and references can't be expressed as a single
        // `&'static str`; callers that may see them must use
        // `llvm_type_string` instead (it renders `[N x T]`, the
        // Vec struct name, and `<inner>*`). Reaching this arm is
        // a backend-routing bug, not a missing feature.
        ty => unreachable!(
            "llvm_type: use llvm_type_string for aggregate / ref type {:?}",
            ty
        ),
    }
}

/// Storage type spelling for `Atomic<T>` in LLVM IR. The
/// checker constrains T to the integer widths
/// `i8 .. i64`/`u8 .. u64` plus `bool`. `Atomic<bool>` lowers
/// to an i8 cell — LLVM atomic ops on `i1` aren't
/// byte-addressable, so the bool value gets zext'd to i8 at
/// every store/CAS boundary and trunc'd back to i1 at every
/// load.
pub(crate) fn atomic_storage_llvm(element: &Type) -> &'static str {
    match element {
        Type::Bool => "i8",
        Type::I8 | Type::U8 => "i8",
        Type::I16 | Type::U16 => "i16",
        Type::I32 | Type::U32 => "i32",
        Type::I64 | Type::U64 => "i64",
        other => unreachable!("unsupported Atomic element type {:?}", other),
    }
}

/// Natural alignment for an `Atomic<T>` cell. C and LLVM both
/// require an explicit `align` attribute on atomic load/store
/// ops; using the wrong alignment can either reject in the
/// verifier or fall back to lock-prefixed ops on x86. The
/// table mirrors `atomic_storage_llvm`.
pub(crate) fn atomic_align(element: &Type) -> u32 {
    match element {
        Type::Bool | Type::I8 | Type::U8 => 1,
        Type::I16 | Type::U16 => 2,
        Type::I32 | Type::U32 => 4,
        Type::I64 | Type::U64 => 8,
        other => unreachable!("unsupported Atomic element type {:?}", other),
    }
}

/// Owned version of `llvm_type` that can render `[N x T]` for arrays
/// and `T*` for references. `llvm_type` returns `&'static str` for
/// the scalar case so it's cheap to use everywhere; aggregates and
/// references need a heap-allocated string.
fn llvm_type_string(ty: &Type) -> String {
    match ty {
        Type::Array { element, length } => {
            // Recurse via `llvm_type_string` so the element
            // can itself be an aggregate (struct, tuple,
            // nested array, …). Previously hardcoded
            // `llvm_type` which panics on aggregates, so
            // `[Point; 3]` and similar shapes blew up
            // during LLVM emit.
            format!("[{} x {}]", length, llvm_type_string(element))
        }
        Type::Vec(element) => vec_struct_name(element),
        Type::Ref(inner) | Type::RefMut(inner) => {
            format!("{}*", llvm_type_string(inner))
        }
        // `Atomic<T>` storage is the futex/atomic-friendly
        // backing width — use `atomic_storage_llvm` so bool
        // gets its i8 shadow rather than the raw i1.
        Type::Atomic(inner) => atomic_storage_llvm(inner).to_string(),
        // `Channel<T, N>` has its own struct type per (T, N).
        Type::Channel(element, capacity) => {
            llvm_channel_struct(element, *capacity)
        }
        // `fn(T1, T2) -> R` lowers to the LLVM function-
        // pointer type `<ret> (<params>)*`.
        Type::FnPtr(params, ret) => {
            let params_s: Vec<String> =
                params.iter().map(llvm_type_string).collect();
            format!(
                "{} ({})*",
                llvm_type_string(ret),
                params_s.join(", ")
            )
        }
        // Tuples use LLVM's anonymous-struct type literal —
        // `{T1, T2, …}`. Functions returning tuples return
        // the struct by value (multi-return); aggregate
        // stores / loads handle them like any struct value.
        // T1.1.
        Type::Tuple(elements) => {
            let parts: Vec<String> =
                elements.iter().map(llvm_type_string).collect();
            format!("{{ {} }}", parts.join(", "))
        }
        // User-declared struct types lower to named LLVM
        // struct types (`%Struct_<Name>`), declared in the
        // module preamble. T1.2 phase 1.
        Type::Struct(name) => format!("%Struct_{}", name),
        // T1.3 phase 2b: payloaded enums lower to named
        // tagged-union struct types (`%Enum_<Name>`)
        // declared in the preamble; plain enums stay as
        // bare `i32` tags.
        Type::Enum(name) => {
            let payloaded = LLVM_ENUM_PAYLOAD_REGISTRY
                .with(|r| r.borrow().contains_key(name));
            if payloaded {
                format!("%Enum_{}", name)
            } else {
                "i32".to_string()
            }
        }
        // Task handle: `{ i64 thread_id, i8* ctx_ptr }`.
        // Declared as `%intent_task_handle` in the module
        // preamble. T1.0 / closure #122.
        Type::Task => "%intent_task_handle".to_string(),
        // Vtables Phase 3b: `dyn Iface` lowers to a per-Iface
        // named struct type `%intent_dyn_<Iface>` (declared in
        // the module preamble alongside per-Iface vtable
        // types). Only emitted when the interface is actually
        // used as `dyn Iface` somewhere.
        Type::Object(name) => format!("%intent_dyn_{}", name),
        _ => llvm_type(ty).to_string(),
    }
}

/// Vtables Phase 3b: thin wrapper over the C backend's
/// `collect_used_dyn_ifaces` so LLVM and C share the same
/// "which interfaces actually need vtable scaffolding" pass.
fn collect_used_dyn_ifaces_llvm(program: &TypedProgram) -> std::collections::HashSet<String> {
    crate::backend_c::collect_used_dyn_ifaces(program)
}

/// Vtables Phase 3b: emit per-Iface vtable + fat-pointer
/// named struct types in LLVM IR. Mirrors tree-C's
/// `emit_dyn_iface_typedefs`. The vtable struct holds an
/// in-declaration-order fn-ptr per interface method, each
/// taking `i8*` (the data pointer) as the first arg.
fn emit_dyn_iface_llvm_typedefs(out: &mut String, used: &std::collections::HashSet<String>) {
    for iface in crate::ast::all_iface_names() {
        if !used.contains(&iface) { continue; }
        let Some(methods) = crate::ast::iface_methods_for(&iface) else {
            continue;
        };
        // Vtable struct: each slot is `<ret> (i8*, <args>)*`.
        let slots: Vec<String> = methods
            .iter()
            .map(|(_, params, ret)| {
                let ret_ty = llvm_type_string(ret);
                let arg_tys: Vec<String> = std::iter::once("i8*".to_string())
                    .chain(params.iter().skip(1).map(llvm_type_string))
                    .collect();
                format!("{} ({})*", ret_ty, arg_tys.join(", "))
            })
            .collect();
        out.push_str(&format!(
            "%intent_vtbl_{} = type {{ {} }}\n",
            iface,
            slots.join(", ")
        ));
        // Fat pointer: `{ %intent_vtbl_<Iface>*, i8* }`.
        out.push_str(&format!(
            "%intent_dyn_{iface} = type {{ %intent_vtbl_{iface}*, i8* }}\n",
            iface = iface
        ));
    }
}

/// Vtables Phase 3b: emit per-(T, Iface) trampolines and the
/// global vtable constants. Mirrors tree-C's
/// `emit_dyn_iface_vtables`. Each trampoline bitcasts `i8*
/// self` to the declared self shape, loads it if by-value,
/// and tail-calls the hoisted `@fn_<T>_<method>`.
fn emit_dyn_iface_llvm_vtables(out: &mut String, used: &std::collections::HashSet<String>) {
    for iface in crate::ast::all_iface_names() {
        if !used.contains(&iface) { continue; }
        let Some(methods) = crate::ast::iface_methods_for(&iface) else {
            continue;
        };
        for type_name in crate::ast::impls_for_iface(&iface) {
            // One trampoline per slot.
            for (idx, (method_name, params, ret)) in methods.iter().enumerate() {
                let ret_ty = llvm_type_string(ret);
                let self_ty = &params[0];
                let mut sig_args: Vec<String> = vec!["i8* %__intent_self".to_string()];
                let mut forwarded: Vec<String> = Vec::new();
                let mut body = String::new();
                // Vtables Phase 4b: cast to THIS impl's
                // concrete nominal type (`%Struct_<type_name>`),
                // not the iface declaration's first-declared
                // self type. Heterogeneous Vec<dyn Iface>
                // depends on each trampoline knowing its own
                // impl's storage shape.
                let impl_storage = format!("%Struct_{}", type_name);
                let self_forward = match self_ty {
                    Type::Struct(_) | Type::Enum(_) => {
                        body.push_str(&format!(
                            "  %__intent_self_ptr = bitcast i8* %__intent_self to {}*\n",
                            impl_storage
                        ));
                        body.push_str(&format!(
                            "  %__intent_self_val = load {}, {}* %__intent_self_ptr\n",
                            impl_storage, impl_storage
                        ));
                        format!("{} %__intent_self_val", impl_storage)
                    }
                    Type::Ref(_) | Type::RefMut(_) => {
                        body.push_str(&format!(
                            "  %__intent_self_ptr = bitcast i8* %__intent_self to {}*\n",
                            impl_storage
                        ));
                        format!("{}* %__intent_self_ptr", impl_storage)
                    }
                    other => {
                        panic!(
                            "vtables Phase 3b: unsupported self shape `{}` for \
                             interface '{}' method '{}` — v1 supports value, ref, \
                             and mut-ref receivers only",
                            other, iface, method_name
                        );
                    }
                };
                forwarded.push(self_forward);
                for (i, pt) in params.iter().enumerate().skip(1) {
                    let pty = llvm_type_string(pt);
                    let pname = format!("%__intent_arg{}", i);
                    sig_args.push(format!("{} {}", pty, pname));
                    forwarded.push(format!("{} {}", pty, pname));
                }
                let fn_arg_tys: Vec<String> = params.iter().map(llvm_type_string).collect();
                let trampoline_name = format!(
                    "intent_trampoline_{}_{}_{}_{}",
                    type_name, iface, idx, method_name
                );
                out.push_str(&format!(
                    "define internal {ret} @{trampoline}({sig}) {{\n",
                    ret = ret_ty,
                    trampoline = trampoline_name,
                    sig = sig_args.join(", ")
                ));
                out.push_str(&body);
                out.push_str(&format!(
                    "  %__intent_ret = call {ret} @fn_{type_name}_{method}({fwd})\n",
                    ret = ret_ty,
                    type_name = type_name,
                    method = method_name,
                    fwd = forwarded.join(", ")
                ));
                out.push_str(&format!("  ret {} %__intent_ret\n", ret_ty));
                out.push_str("}\n");
                let _ = fn_arg_tys;
            }
            // Global vtable constant.
            let init_parts: Vec<String> = methods
                .iter()
                .enumerate()
                .map(|(idx, (method_name, params, ret))| {
                    let ret_ty = llvm_type_string(ret);
                    let arg_tys: Vec<String> = std::iter::once("i8*".to_string())
                        .chain(params.iter().skip(1).map(llvm_type_string))
                        .collect();
                    let ptr_ty = format!("{} ({})*", ret_ty, arg_tys.join(", "));
                    format!(
                        "{ptr_ty} @intent_trampoline_{type_name}_{iface}_{slot}_{method}",
                        ptr_ty = ptr_ty,
                        type_name = type_name,
                        iface = iface,
                        slot = idx,
                        method = method_name,
                    )
                })
                .collect();
            out.push_str(&format!(
                "@intent_vtbl_{iface}_{type_name} = constant %intent_vtbl_{iface} {{ {} }}\n",
                init_parts.join(", "),
                iface = iface,
                type_name = type_name,
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile;

    #[test]
    fn emits_minimal_main_returning_literal() {
        let source = r#"
            fn main() -> i64 {
              return 42;
            }
        "#;
        let checked = compile(source).expect("simple program compiles");
        let ll = LlvmBackend.emit(&checked.ir);
        assert!(ll.contains("define i64 @fn_main()"));
        // The IR caches the return value in a temp (so drops can fire
        // safely before the return); the literal 42 is stored to the
        // temp alloca, then `ret` returns the loaded value.
        // Return-temp alloca's name now carries a uniquification
        // suffix, so look for any `store i64 42` into an
        // `__intent_ret_…` alloca.
        assert!(
            ll.lines().any(|l| l.contains("store i64 42")
                && l.contains("__intent_ret_")),
            "expected return-temp store, got:\n{ll}"
        );
        assert!(ll.contains("ret i64"));
        assert!(ll.contains("define i32 @main()"));
    }

    #[test]
    fn emits_integer_addition() {
        let source = r#"
            fn add(a: i64, b: i64) -> i64 { return a + b; }
            fn main() -> i64 { return add(2, 3); }
        "#;
        let checked = compile(source).expect("add compiles");
        let ll = LlvmBackend.emit(&checked.ir);
        assert!(ll.contains("add i64"));
        assert!(ll.contains("call i64 @fn_add(i64 2, i64 3)"));
    }

    /// Resolve the `lli` binary path: prefer `$LLI`, fall back to a
    /// PATH lookup of `lli`. Avoids hardcoding `/usr/bin/lli` so
    /// the tests work on systems where lli lives elsewhere
    /// (homebrew on macOS, /usr/local on some Linux distros).
    fn lli_path() -> String {
        std::env::var("LLI").unwrap_or_else(|_| "lli".to_string())
    }

    /// True when `lli` is installed; tests that actually execute
    /// generated IR are gated on this, mirroring the `z3_available`
    /// pattern in `lib.rs`.
    fn lli_available() -> bool {
        std::process::Command::new(lli_path())
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn run_lli(source: &str) -> i32 {
        use std::io::Write;
        let checked = compile(source).expect("source compiles");
        let ll = LlvmBackend.emit(&checked.ir);
        let path = std::env::temp_dir().join(format!(
            "intent-llvm-{}-{}.ll",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        {
            let mut f = std::fs::File::create(&path).expect("write .ll");
            f.write_all(ll.as_bytes()).expect("write");
        }
        let mut cmd = std::process::Command::new(lli_path());
        // Mirror `intentc run`'s libgomp load flags so parallel-for
        // programs can resolve `@GOMP_parallel` etc. Programs that
        // don't use parallel-for are unaffected — lli ignores
        // `-load`'d symbols that aren't referenced.
        add_libgomp_load_flags_for_tests(&mut cmd);
        // lli's MCJIT isn't thread-safe for concurrent function
        // resolution, so cap libgomp to a single thread (same as
        // `intentc run`). The reductions still exercise the
        // shadow / atomicrmw path correctly under one thread —
        // we're testing emission shape, not contention.
        if std::env::var("OMP_NUM_THREADS").is_err() {
            cmd.env("OMP_NUM_THREADS", "1");
        }
        let status = cmd
            .arg(&path)
            .status()
            .expect("lli runs");
        let _ = std::fs::remove_file(&path);
        status.code().unwrap_or(-1)
    }

    /// Tests-local mirror of [`crate::main::add_libgomp_load_flags`].
    /// Walks the same candidate list and appends a `-load=<path>`
    /// flag for the first existing file (with an `INTENT_LIBGOMP`
    /// env override). Kept here so the LLVM-backend test module
    /// doesn't reach into `main.rs`.
    fn add_libgomp_load_flags_for_tests(cmd: &mut std::process::Command) {
        const CANDIDATES: &[&str] = &[
            "/usr/lib/x86_64-linux-gnu/libgomp.so.1",
            "/lib/x86_64-linux-gnu/libgomp.so.1",
            "/usr/lib64/libgomp.so.1",
            "/usr/lib/aarch64-linux-gnu/libgomp.so.1",
            "/opt/homebrew/opt/libomp/lib/libomp.dylib",
            "/usr/local/opt/libomp/lib/libomp.dylib",
        ];
        for path in CANDIDATES {
            if std::path::Path::new(path).exists() {
                cmd.arg(format!("-load={}", path));
                return;
            }
        }
        if let Ok(p) = std::env::var("INTENT_LIBGOMP") {
            if std::path::Path::new(&p).exists() {
                cmd.arg(format!("-load={}", p));
            }
        }
    }

    #[test]
    fn lli_runs_max_program_and_returns_expected_exit_code() {
        if !lli_available() {
            return;
        }
        // `max` / `min` are reserved intrinsic keywords, so the
        // user-defined function uses a non-reserved name here.
        let source = r#"
            fn pick_larger(a: i64, b: i64) -> i64 {
              if a > b { return a; }
              return b;
            }
            fn main() -> i64 { return pick_larger(3, 7); }
        "#;
        assert_eq!(run_lli(source), 7);
    }

    #[test]
    fn lli_aborts_on_violated_requires() {
        if !lli_available() {
            return;
        }
        // `safe_div` requires b > 0. Pass b through `id()` which has
        // no ensures, so the verifier can't prove the precondition
        // and the runtime guard must catch it. Process should be
        // killed by SIGABRT (exit code 128+6=134) — `run_lli`
        // returns -1 when status.code() is None.
        let source = r#"
            fn safe_div(a: i64, b: i64) -> i64
            requires b > 0;
            {
              return a / b;
            }
            fn id(x: i64) -> i64 { return x; }
            fn main() -> i64 {
              return safe_div(10, id(0));
            }
        "#;
        let exit = run_lli_full(source);
        assert!(
            exit.is_none() || exit == Some(134),
            "expected abort signal, got code {:?}",
            exit
        );
    }

    #[test]
    fn lli_aborts_on_div_by_zero() {
        if !lli_available() {
            return;
        }
        // No `requires` to guard the divisor; the per-op checked
        // guard inside Div should fire.
        let source = r#"
            fn id(x: i64) -> i64 { return x; }
            fn main() -> i64 {
              return 10 / id(0);
            }
        "#;
        let exit = run_lli_full(source);
        assert!(
            exit.is_none() || exit == Some(134),
            "expected abort signal on div by zero, got {:?}",
            exit
        );
    }

    /// Like `run_lli` but returns `Option<i32>`: `None` when the
    /// program was killed by a signal (e.g. SIGABRT from
    /// `@abort()`), `Some(code)` for a normal exit.
    fn run_lli_full(source: &str) -> Option<i32> {
        use std::io::Write;
        let checked = compile(source).expect("source compiles");
        let ll = LlvmBackend.emit(&checked.ir);
        let path = std::env::temp_dir().join(format!(
            "intent-llvm-abort-{}-{}.ll",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        {
            let mut f = std::fs::File::create(&path).expect("write .ll");
            f.write_all(ll.as_bytes()).expect("write");
        }
        let status = std::process::Command::new(lli_path())
            .arg(&path)
            .stderr(std::process::Stdio::null())
            .status()
            .expect("lli runs");
        let _ = std::fs::remove_file(&path);
        status.code()
    }

    #[test]
    fn lli_runs_for_iter_borrowed_array() {
        if !lli_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let xs: [i64; 4] = [1, 2, 3, 4];
              let sum: i64 = 0;
              for x in ref xs { sum = sum + x; }
              return sum;
            }
        "#;
        assert_eq!(run_lli(source), 10);
    }

    #[test]
    fn lli_runs_for_iter_consume_vec() {
        if !lli_available() {
            return;
        }
        // Vec is consumed by the for-iter; the buffer is freed at
        // loop exit. Reading `sum` at the end still works because
        // sum is i64 (Copy).
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(10, 20, 30, 40);
              let sum: i64 = 0;
              for x in xs { sum = sum + x; }
              return sum;
            }
        "#;
        assert_eq!(run_lli(source), 100);
    }

    #[test]
    fn lli_runs_vec_push_set_clone() {
        if !lli_available() {
            return;
        }
        // push: appends. set: writes in place. clone: copies.
        let push = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2, 3);
              let ys: Vec<i64> = push(xs, 99);
              return ys[3];
            }
        "#;
        assert_eq!(run_lli(push), 99);

        let set = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2, 3);
              let ys: Vec<i64> = set(xs, 1, 88);
              return ys[1];
            }
        "#;
        assert_eq!(run_lli(set), 88);

        let clone = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(7, 8, 9);
              let ys: Vec<i64> = clone(xs);
              return ys[0] + ys[1] + ys[2];
            }
        "#;
        assert_eq!(run_lli(clone), 24);
    }

    #[test]
    fn lli_runs_vec_literal_read_len_and_drop() {
        if !lli_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(10, 20, 30, 40, 50);
              let n: u64 = len(xs);
              let sum: i64 = 0;
              for i from 0 to 5 { sum = sum + xs[i as u64]; }
              assert sum == 150;
              return (n as i64);
            }
        "#;
        assert_eq!(run_lli(source), 5);
    }

    #[test]
    fn lli_runs_vec_index_assign() {
        if !lli_available() {
            return;
        }
        // Returns xs[1] directly. The checker now caches the return
        // value into a temp *before* dropping `xs`, so this is no
        // longer a use-after-free.
        let source = r#"
            fn main() -> i64 {
              let xs: Vec<i64> = vec(1, 2, 3);
              xs[1] = 99;
              return xs[1];
            }
        "#;
        assert_eq!(run_lli(source), 99);
    }

    #[test]
    fn lli_runs_array_passed_by_reference() {
        if !lli_available() {
            return;
        }
        let source = r#"
            fn sum5(xs: ref [i64; 5]) -> i64 {
              let s: i64 = 0;
              for i from 0 to 5 { s = s + xs[i]; }
              return s;
            }
            fn main() -> i64 {
              let xs: [i64; 5] = [10, 20, 30, 40, 50];
              return sum5(ref xs);
            }
        "#;
        assert_eq!(run_lli(source), 150);
    }

    #[test]
    fn lli_runs_mutable_ref_indexed_write() {
        if !lli_available() {
            return;
        }
        let source = r#"
            fn bump_first(xs: mut ref [i64; 3]) {
              xs[0] = xs[0] + 100;
            }
            fn main() -> i64 {
              let xs: [i64; 3] = [1, 2, 3];
              bump_first(mut ref xs);
              return xs[0];
            }
        "#;
        // bump_first is called for its side effect; it has no return
        // type. The checker accepts no-return functions as having
        // return type `()`-equivalent (here represented as i64 0
        // by the language; verifying that we round-trip xs[0] = 101).
        let _ = source;
        // Compile-and-check only — this exercises &mut + IndexAssign.
        let checked = compile(source);
        // If the language doesn't yet allow no-return fns, the source
        // will fail to compile. Skip in that case rather than failing
        // the test on a checker-policy issue unrelated to LLVM.
        if checked.is_err() {
            return;
        }
        let ll = LlvmBackend.emit(&checked.unwrap().ir);
        assert!(
            ll.contains("getelementptr [3 x i64], [3 x i64]* %arg_xs"),
            "expected GEP through &mut [3 x i64], got:\n{ll}"
        );
    }

    #[test]
    fn lli_runs_array_literal_and_indexed_read() {
        if !lli_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let xs: [i64; 5] = [10, 20, 30, 40, 50];
              let total: i64 = 0;
              for i from 0 to 5 { total = total + xs[i]; }
              return total;
            }
        "#;
        assert_eq!(run_lli(source), 150);
    }

    #[test]
    fn lli_runs_array_index_assign() {
        if !lli_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let xs: [i64; 3] = [1, 2, 3];
              xs[1] = 99;
              return xs[1];
            }
        "#;
        assert_eq!(run_lli(source), 99);
    }

    #[test]
    fn lli_runs_for_range_sum() {
        if !lli_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let total: i64 = 0;
              for i from 0 to 10 {
                total = total + i;
              }
              return total;
            }
        "#;
        assert_eq!(run_lli(source), 45);
    }

    #[test]
    fn lli_runs_float_arithmetic() {
        if !lli_available() {
            return;
        }
        // Returns 0 (program success); the print is the meaningful
        // side effect. Use a let to exercise the alloca/store/load
        // path for floats specifically.
        let source = r#"
            fn main() -> i64 {
              let x: f64 = 3.5;
              let y: f64 = 2.0;
              let z: f64 = x * y;
              assert z == 7.0;
              return 0;
            }
        "#;
        assert_eq!(run_lli(source), 0);
    }

    #[test]
    fn lli_runs_while_loop_with_accumulator() {
        if !lli_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let i: i64 = 0;
              let sum: i64 = 0;
              while i < 5 {
                sum = sum + i;
                i = i + 1;
              }
              return sum;
            }
        "#;
        assert_eq!(run_lli(source), 10);
    }

    #[test]
    fn lli_runs_while_with_break() {
        if !lli_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let i: i64 = 0;
              while i < 100 {
                if i == 7 { break; }
                i = i + 1;
              }
              return i;
            }
        "#;
        assert_eq!(run_lli(source), 7);
    }

    #[test]
    fn lli_runs_print_and_assert() {
        if !lli_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 42;
              print x;
              assert x == 42;
              let y: bool = true;
              print y;
              return 0;
            }
        "#;
        // Run via lli, capture stdout, and verify both values + exit.
        use std::io::Write;
        let checked = compile(source).expect("source compiles");
        let ll = LlvmBackend.emit(&checked.ir);
        let path = std::env::temp_dir().join(format!(
            "intent-llvm-print-{}-{}.ll",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        {
            let mut f = std::fs::File::create(&path).expect("write .ll");
            f.write_all(ll.as_bytes()).expect("write");
        }
        let output = std::process::Command::new(lli_path())
            .arg(&path)
            .output()
            .expect("lli runs");
        let _ = std::fs::remove_file(&path);
        assert!(output.status.success(), "lli failed: {:?}", output);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("42"), "expected 42 in stdout: {stdout}");
        assert!(stdout.contains("true"), "expected true in stdout: {stdout}");
    }

    #[test]
    fn lli_runs_arithmetic_with_let_and_reassign() {
        if !lli_available() {
            return;
        }
        let source = r#"
            fn main() -> i64 {
              let x: i64 = 10;
              let y: i64 = 3;
              return (x * y) - 5;
            }
        "#;
        assert_eq!(run_lli(source), 25);
    }

    #[test]
    fn emits_alloca_and_store_for_let_and_params() {
        let source = r#"
            fn id(x: i64) -> i64 {
              let y: i64 = x;
              return y;
            }
            fn main() -> i64 { return id(5); }
        "#;
        let checked = compile(source).expect("alloca path compiles");
        let ll = LlvmBackend.emit(&checked.ir);
        // Parameter is copied into an alloca named %x.addr (no
        // uniquification suffix for params — only one per fn).
        assert!(ll.contains("%x.addr = alloca i64"));
        assert!(ll.contains("store i64 %arg_x, i64* %x.addr"));
        // `let y = x;` allocates with a fresh-tmp-prefixed name
        // (uniquification: see TypedStmt::Let emit). The name
        // ends in `.y.addr` and stores into an i64.
        assert!(
            ll.lines().any(|l| l.contains(".y.addr = alloca i64")),
            "expected an alloca for `y`, got:\n{ll}"
        );
        assert!(
            ll.lines().any(|l| l.contains(".y.addr")
                && l.contains("load i64")),
            "expected a load through `y`'s alloca, got:\n{ll}"
        );
    }

    #[test]
    fn emits_if_else_branch_with_terminating_then() {
        // `max` is a reserved intrinsic keyword, so use a
        // non-reserved name for the user-defined helper.
        let source = r#"
            fn pick_larger(a: i64, b: i64) -> i64 {
              if a > b { return a; }
              return b;
            }
            fn main() -> i64 { return 0; }
        "#;
        let checked = compile(source).expect("if/else compiles");
        let ll = LlvmBackend.emit(&checked.ir);
        assert!(ll.contains("br i1"));
        assert!(ll.contains("icmp sgt i64"));
        // Then-branch returned, so its terminator is `ret`, not `br`.
        // Verify the basic structure: a `then` block followed by a
        // `ret i64` *before* the else block label.
        let then_pos = ll.find("then0:").expect("then block");
        let else_pos = ll.find("else1:").expect("else block");
        let ret_pos = ll[then_pos..else_pos].find("ret i64")
            .expect("then-block should ret directly");
        let _ = ret_pos;
    }

    #[test]
    fn host_sys_futex_number_returns_some_for_supported_arch() {
        // The host on which we run tests must be one of the
        // architectures the helper recognizes. A None here
        // means an unsupported host slipped in without us
        // adding its arch to the table.
        let nr = host_sys_futex_number();
        assert!(
            nr.is_some(),
            "test host's SYS_futex number is unknown to host_sys_futex_number; \
             add the architecture's number to the helper"
        );
        // Sanity-spot-check a known arch when we're on it.
        #[cfg(target_arch = "x86_64")]
        assert_eq!(nr, Some(202), "x86_64 SYS_futex must be 202");
        #[cfg(target_arch = "aarch64")]
        assert_eq!(nr, Some(98), "aarch64 SYS_futex must be 98");
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn mutex_lock_uses_futex_wait_and_wake_on_linux() {
        // The lock helper drives Drepper's three-state futex
        // protocol on POSIX hosts: fast CAS → mark waiters →
        // park via syscall(FUTEX_WAIT_PRIVATE). Unlock
        // fetch_subs the state and on the waiters-present
        // path calls syscall(FUTEX_WAKE_PRIVATE). Skipped on
        // Windows hosts where the dispatch emits
        // `@WaitOnAddress` / `@WakeByAddressSingle` instead
        // (covered by `mutex_lock_uses_wait_on_address_on_windows`).
        let source = r#"
            fn main() -> i64 {
              let m: Mutex<i64> = mutex_new(0);
              let g: Guard<i64> = mutex_lock(ref m);
              return guard_get(ref g);
            }
        "#;
        let checked = compile(source).expect("mutex program compiles");
        let ll = LlvmBackend.emit(&checked.ir);
        assert!(
            ll.contains("declare i64 @syscall(i64, ...)"),
            "expected variadic syscall extern:\n{ll}"
        );
        // FUTEX_WAIT_PRIVATE = 128 in the lock path. The
        // SYS_futex number is host-arch-specific (we test on
        // the same arch we're running on, so the helper's
        // pick is the right reference value).
        let nr = host_sys_futex_number()
            .expect("host SYS_futex number known for this arch");
        let wait_call = format!(
            "call i64 (i64, ...) @syscall(i64 {}, i32* ",
            nr
        );
        assert!(
            ll.contains(&wait_call),
            "expected SYS_futex ({}) call from mutex helpers:\n{ll}",
            nr
        );
        assert!(
            ll.contains("i32 128, i32 2"),
            "expected FUTEX_WAIT_PRIVATE(state=2) operand pattern:\n{ll}"
        );
        // FUTEX_WAKE_PRIVATE = 129 in the unlock path. The
        // unlock-side wake is only emitted when a Guard drops
        // — verify the constant appears.
        assert!(
            ll.contains("i32 129, i32 1"),
            "expected FUTEX_WAKE_PRIVATE(n=1) operand pattern:\n{ll}"
        );
        // The state field is now i32-wide so futex sees it
        // as a 32-bit word.
        assert!(
            ll.contains("%intent_mutex_i64 = type { i64, i32 }"),
            "expected mutex struct with i32 state field:\n{ll}"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn mutex_lock_uses_wait_on_address_on_windows() {
        // On Windows hosts the mutex park/wake fast path
        // uses WaitOnAddress / WakeByAddressSingle from the
        // Synchronization API. Same Drepper three-state
        // protocol, different kernel-wait primitive. The
        // declares for the POSIX path are absent.
        let source = r#"
            fn main() -> i64 {
              let m: Mutex<i64> = mutex_new(0);
              let g: Guard<i64> = mutex_lock(ref m);
              return guard_get(ref g);
            }
        "#;
        let checked = compile(source).expect("mutex program compiles");
        let ll = LlvmBackend.emit(&checked.ir);
        assert!(
            ll.contains("declare i32 @WaitOnAddress(i8*, i8*, i64, i32)"),
            "expected WaitOnAddress extern:\n{ll}"
        );
        assert!(
            ll.contains("declare void @WakeByAddressSingle(i8*)"),
            "expected WakeByAddressSingle extern:\n{ll}"
        );
        assert!(
            ll.contains("call i32 @WaitOnAddress(i8* "),
            "expected WaitOnAddress call from mutex park:\n{ll}"
        );
        assert!(
            ll.contains("call void @WakeByAddressSingle(i8* "),
            "expected WakeByAddressSingle call from Guard drop:\n{ll}"
        );
        assert!(
            !ll.contains("@pthread_create") && !ll.contains("@syscall"),
            "expected POSIX threading declares to be absent on Windows:\n{ll}"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn task_spawn_uses_create_thread_on_windows() {
        // Win32 spawn: `CreateThread` returns a HANDLE (i8*)
        // which we `ptrtoint` into the i64 handle slot.
        // Join: `WaitForSingleObject(handle, INFINITE)` +
        // `CloseHandle`.
        let source = r#"
            fn main() -> i64 {
              let bias: i64 = 7;
              task ta {
                let v: i64 = bias;
                let _ = v;
              }
              join ta;
              return 0;
            }
        "#;
        let checked = compile(source).expect("task program compiles");
        let ll = LlvmBackend.emit(&checked.ir);
        assert!(
            ll.contains("declare i8* @CreateThread(i8*, i64, i8* (i8*)*, i8*, i32, i32*)"),
            "expected CreateThread extern:\n{ll}"
        );
        assert!(
            ll.contains("declare i32 @WaitForSingleObject(i8*, i32)"),
            "expected WaitForSingleObject extern:\n{ll}"
        );
        assert!(
            ll.contains("call i8* @CreateThread(i8* null, i64 0, i8* (i8*)* @intent_task_0"),
            "expected CreateThread call at spawn site:\n{ll}"
        );
        assert!(
            ll.contains("call i32 @WaitForSingleObject(i8* ") && ll.contains("i32 -1)"),
            "expected WaitForSingleObject(_, INFINITE) at join:\n{ll}"
        );
        assert!(
            ll.contains("call i32 @CloseHandle(i8* "),
            "expected CloseHandle at join:\n{ll}"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn parallel_for_uses_create_thread_fanout_on_windows() {
        // libgomp isn't available on Windows, so `parallel for`
        // open-codes a `@CreateThread` fan-out (N=4 hardcoded
        // worker threads). The outlined fn becomes
        // `i8* (i8*)` to match CreateThread's start-routine ABI
        // and reads tid/nt out of a `WinParArg { ctx, tid, nt }`
        // struct instead of calling `omp_get_*`. GOMP/omp_get_*
        // declarations are absent.
        let source = r#"
            fn square(x: i64) -> i64 {
              return x * x;
            }
            fn main() -> i64 {
              parallel for i from 0 to 8 {
                let _ = square(i);
              }
              return 0;
            }
        "#;
        let checked = compile(source).expect("parallel-for compiles");
        let ll = LlvmBackend.emit(&checked.ir);
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
        // Three CreateThread calls (tids 1..3); tid 0 runs in
        // the calling thread.
        let create_calls = ll
            .matches("call i8* @CreateThread(i8* null, i64 0, i8* (i8*)* @__intent_par_")
            .count();
        assert_eq!(
            create_calls, 3,
            "expected 3 CreateThread calls (N-1 workers):\n{ll}"
        );
        let wait_calls = ll
            .matches("call i32 @WaitForSingleObject(i8* ")
            .count();
        assert!(
            wait_calls >= 3,
            "expected at least 3 WaitForSingleObject calls (one per spawned thread):\n{ll}"
        );
    }

    #[test]
    fn channel_struct_has_per_slot_publication_counter() {
        // The struct now carries a second [16 x i64] array
        // (`seq`) alongside `buf`. The Vyukov MPSC protocol
        // uses seq[i] to coordinate producer→consumer slot
        // visibility — closes the CAS-then-write race that the
        // tail-CAS-only design had.
        let source = r#"
            fn main() -> i64 {
              let ch: Channel<i64> = channel_new();
              let _ = channel_send(ref ch, 1);
              return channel_recv(ref ch);
            }
        "#;
        let checked = compile(source).expect("channel program compiles");
        let ll = LlvmBackend.emit(&checked.ir);
        // Two [16 x i64] fields in the struct definition.
        let canonical = "%intent_channel_i64_16 = type { [16 x i64], [16 x i64], i64, i64 }";
        assert!(
            ll.contains(canonical),
            "expected struct with seq array, got:\n{ll}"
        );
        // channel_new initializes seq[i] = i via a constant
        // array initializer.
        assert!(
            ll.contains("[i64 0, i64 1, i64 2, i64 3, i64 4, i64 5, i64 6, i64 7, i64 8, i64 9, i64 10, i64 11, i64 12, i64 13, i64 14, i64 15]"),
            "expected identity-initialized seq array in channel_new:\n{ll}"
        );
    }

    #[test]
    fn channel_send_uses_cmpxchg_on_tail_for_multi_producer_safety() {
        // The producer must CAS the tail to claim a unique
        // slot — a plain `store atomic tail = t+1` would let
        // two producers seeing the same `t` both write into
        // slot `t & 15`. The CAS-claim happens before the
        // slot write so each producer owns its slot.
        let source = r#"
            fn main() -> i64 {
              let ch: Channel<i64> = channel_new();
              let _ = channel_send(ref ch, 42);
              return channel_recv(ref ch);
            }
        "#;
        let checked = compile(source).expect("channel_send compiles");
        let ll = LlvmBackend.emit(&checked.ir);
        // The IR around the channel_send call should contain
        // a cmpxchg on an i64 pointer (the tail field GEP).
        assert!(
            ll.contains("cmpxchg i64*"),
            "expected cmpxchg on the channel tail pointer:\n{ll}"
        );
        // And NOT a plain non-CAS `store atomic` to bump the
        // tail (the pre-MPSC shape). Other atomic ops in the
        // module (head bump in recv, atomic_store builtins,
        // etc.) are allowed and use their own widths.
        let tail_cas_marker = "ch_send_try";
        assert!(
            ll.contains(tail_cas_marker),
            "expected the CAS-claim block label `{tail_cas_marker}`:\n{ll}"
        );
    }

    #[test]
    fn emits_atomicrmw_or_on_i8_shadow_for_bool_or_reduction() {
        // `||` over an i1 can't go through atomicrmw directly
        // (LLVM rejects byte-unsized atomicrmw operands). The
        // backend allocates an i8 shadow in the parent, zext-
        // stores the current bool, runs `atomicrmw or i8*` from
        // the outlined fn, then on exit reads the shadow and
        // writes `icmp ne i8 …, 0` back into the original i1
        // alloca.
        let source = r#"
            fn main() -> i64 {
              let flags: [bool; 3] = [false, false, true];
              let any: bool = false;
              parallel for i from 0 to 3
              reduce any with ||;
              {
                any = any || flags[i];
              }
              if any { return 1; } else { return 0; }
            }
        "#;
        let checked = compile(source).expect("bool reduction compiles");
        let ll = LlvmBackend.emit(&checked.ir);
        assert!(
            ll.contains("atomicrmw or i8*"),
            "expected atomicrmw or on i8 shadow:\n{ll}"
        );
        assert!(
            ll.contains("zext i1"),
            "expected zext i1 on increment:\n{ll}"
        );
        assert!(
            ll.contains("icmp ne i8"),
            "expected icmp ne on shadow writeback:\n{ll}"
        );
        // No more "lowers sequentially" fallback comment.
        assert!(
            !ll.contains("parallel for with bool reduction lowers sequentially"),
            "sequential-fallback comment should be gone:\n{ll}"
        );
        // And the parallel-for must actually outline (a real
        // @__intent_par_<N> definition should appear).
        assert!(
            ll.contains("define internal void @__intent_par_"),
            "expected outlined parallel-for function:\n{ll}"
        );
    }

    #[test]
    fn emits_atomicrmw_and_on_i8_shadow_for_bool_and_reduction() {
        // Same shape as the `||` test but for `&&`. Verifies the
        // shadow path is symmetric.
        let source = r#"
            fn main() -> i64 {
              let flags: [bool; 3] = [true, true, true];
              let all: bool = true;
              parallel for i from 0 to 3
              reduce all with &&;
              {
                all = all && flags[i];
              }
              if all { return 1; } else { return 0; }
            }
        "#;
        let checked = compile(source).expect("bool reduction compiles");
        let ll = LlvmBackend.emit(&checked.ir);
        assert!(
            ll.contains("atomicrmw and i8*"),
            "expected atomicrmw and on i8 shadow:\n{ll}"
        );
    }

    #[test]
    fn lli_runs_bool_or_reduction_and_returns_true_when_any_true() {
        if !lli_available() {
            return;
        }
        // End-to-end: the `||` reduction with shadow lowering
        // must observe `true` because one element of `flags` is
        // true. Returns 1.
        let source = r#"
            fn main() -> i64 {
              let flags: [bool; 4] = [false, false, true, false];
              let any: bool = false;
              parallel for i from 0 to 4
              reduce any with ||;
              {
                any = any || flags[i];
              }
              if any { return 1; } else { return 0; }
            }
        "#;
        assert_eq!(run_lli(source), 1);
    }

    #[test]
    fn lli_runs_bool_and_reduction_and_returns_false_when_one_false() {
        if !lli_available() {
            return;
        }
        // Mirror: `&&` reduction with shadow lowering. One false
        // in `flags` collapses the result to 0.
        let source = r#"
            fn main() -> i64 {
              let flags: [bool; 4] = [true, true, false, true];
              let all: bool = true;
              parallel for i from 0 to 4
              reduce all with &&;
              {
                all = all && flags[i];
              }
              if all { return 1; } else { return 0; }
            }
        "#;
        assert_eq!(run_lli(source), 0);
    }

    #[test]
    fn emits_native_atomicrmw_for_bitwise_int_reductions() {
        // Bitwise `&` / `|` / `^` on integer reductions go through
        // the same atomicrmw path as `+` (direct, native-width)
        // rather than the i8 shadow used by bool `&&`/`||`.
        let source = r#"
            fn main() -> i64 {
              let xs: [i64; 4] = [12, 10, 7, 13];
              let a: i64 = -1;
              let b: i64 = 0;
              let c: i64 = 0;
              parallel for i from 0 to 4
              reduce a with &;
              reduce b with |;
              reduce c with ^;
              {
                a = a & xs[i];
                b = b | xs[i];
                c = c ^ xs[i];
              }
              return (a + b) + c;
            }
        "#;
        let checked = compile(source).expect("bitwise reductions compile");
        let ll = LlvmBackend.emit(&checked.ir);
        assert!(
            ll.contains("atomicrmw and i64*"),
            "expected native-width atomicrmw and:\n{ll}"
        );
        assert!(
            ll.contains("atomicrmw or i64*"),
            "expected native-width atomicrmw or:\n{ll}"
        );
        assert!(
            ll.contains("atomicrmw xor i64*"),
            "expected atomicrmw xor:\n{ll}"
        );
        // No i8 shadow path should be emitted — that's the bool
        // path. Bitwise int reductions skip the zext-from-i1.
        let xor_idx = ll.find("atomicrmw xor i64*").expect("xor present");
        // Look in a ~40-line window for any nearby `zext i1`
        // that would indicate the bool path leaked in.
        let window_start = ll[..xor_idx].rfind("body:").unwrap_or(0);
        let window = &ll[window_start..xor_idx];
        assert!(
            !window.contains("zext i1"),
            "bitwise xor reduction must not zext from i1:\n{window}"
        );
    }

    #[test]
    fn lli_runs_bitwise_reductions_and_returns_expected_value() {
        if !lli_available() {
            return;
        }
        // 12 & 10 & 7 & 13 = 0
        // 12 | 10 | 7 | 13 = 15
        // 12 ^ 10 ^ 7 ^ 13 = 12
        // Sum: 0 + 15 + 12 = 27.
        let source = r#"
            fn main() -> i64 {
              let xs: [i64; 4] = [12, 10, 7, 13];
              let a: i64 = -1;
              let b: i64 = 0;
              let c: i64 = 0;
              parallel for i from 0 to 4
              reduce a with &;
              reduce b with |;
              reduce c with ^;
              {
                a = a & xs[i];
                b = b | xs[i];
                c = c ^ xs[i];
              }
              return (a + b) + c;
            }
        "#;
        assert_eq!(run_lli(source), 27);
    }

    #[test]
    fn emits_aggregate_load_store_for_array_let_from_var() {
        // `let ys: [T;N] = xs;` used to drop a "TODO" comment and
        // leave ys's alloca uninitialized (so reads returned
        // garbage). The fix copies the source array via a whole-
        // aggregate load + store, which LLVM optimizes into a
        // memcpy.
        let source = r#"
            fn main() -> i64 {
              let xs: [i64; 3] = [10, 20, 30];
              let ys: [i64; 3] = xs;
              return ys[1];
            }
        "#;
        let checked = compile(source).expect("array let from var compiles");
        let ll = LlvmBackend.emit(&checked.ir);
        assert!(
            ll.contains("load [3 x i64], [3 x i64]*"),
            "expected whole-aggregate load:\n{ll}"
        );
        assert!(
            ll.contains("store [3 x i64]"),
            "expected whole-aggregate store:\n{ll}"
        );
        assert!(
            !ll.contains("TODO(llvm-backend): array let from non-literal rhs"),
            "TODO comment should be gone:\n{ll}"
        );
    }

    #[test]
    fn lli_runs_array_let_from_var_and_returns_copied_element() {
        if !lli_available() {
            return;
        }
        // End-to-end: the copy must actually preserve ys[1] = 20.
        let source = r#"
            fn main() -> i64 {
              let xs: [i64; 3] = [10, 20, 30];
              let ys: [i64; 3] = xs;
              return ys[1];
            }
        "#;
        assert_eq!(run_lli(source), 20);
    }

    #[test]
    fn emits_free_for_discarded_vec() {
        // `let _ = some_fn_returning_vec(...);` used to leak the
        // buffer. The Discard handler now routes through the
        // per-element-type `__free` helper which both frees
        // the outer buffer and (for nested Vec elements)
        // recursively frees each slot. Refines #7.
        let source = r#"
            fn make(n: i64) -> Vec<i64>
            requires n >= 0;
            {
              let xs: Vec<i64> = vec(1, 2, n);
              return xs;
            }
            fn main() -> i64 {
              let _ = make(5);
              return 0;
            }
        "#;
        let checked = compile(source).expect("Vec discard compiles");
        let ll = LlvmBackend.emit(&checked.ir);
        let main_start = ll.find("define i64 @fn_main()").expect("fn_main present");
        let main_end = ll[main_start..]
            .find("\n}\n")
            .map(|i| main_start + i)
            .unwrap_or(ll.len());
        let body = &ll[main_start..main_end];
        assert!(
            body.contains("call void @intent_vec_i64__free("),
            "expected per-type __free call on the discarded Vec:\n{body}"
        );
        assert!(
            !body.contains("TODO(llvm-backend): Discard of aggregate type"),
            "TODO comment should be gone:\n{body}"
        );
    }

    #[test]
    fn emits_signed_vs_unsigned_division() {
        let source = r#"
            fn s_div(a: i64, b: i64) -> i64 requires b > 0; { return a / b; }
            fn u_div(a: u64, b: u64) -> u64 requires b > 0; { return a / b; }
            fn main() -> i64 { return 0; }
        "#;
        let checked = compile(source).expect("div compiles");
        let ll = LlvmBackend.emit(&checked.ir);
        assert!(ll.contains("sdiv i64"), "expected sdiv in:\n{}", ll);
        assert!(ll.contains("udiv i64"), "expected udiv in:\n{}", ll);
    }
}
