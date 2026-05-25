# vāṇī (वाणी) — pending items

Snapshot from 2026-05-18 after min/max reductions + parallelism docs
refresh landed. Order is rough priority (size + payoff), not strict.

## ⏳ Resume here (paused 2026-05-25, after closure #198)

Closures landed: #99 bounded generics, #100 affine struct
fields broadened, #101 user-Drop auto-call, #102 field-borrow
expressions, #103 reverse-declaration field drop order, #104
user-Eq desugar for struct `==`, #105 partial-move tracking,
#106 enum `==` desugar + partial-then-whole-move diagnostic,
#107 tuple auto-equality, #108 in-place
`push(mut ref xs, v)`, #109 `xs[i].field = v` mixed-place
assignment, #110 match on bool scrutinee, #111 match on Str
scrutinee, #112 deep field paths for mixed-place assign,
#113 enum payloads admit OwnedStr (heap-aware Drop), #114
type-associated functions (`Type.helper(args)`), #115
unit-return functions, #116 empty struct + bare-block
scope-stmt, #117 SSA bool-print parity, #118 Vec<T> enum
payload, #119 [T;N] enum payload, #120 const-N as array
length, #121 const-initializer arithmetic, #122
Task + Atomic enum payloads, #123 Mutex / Channel
struct fields, #124 Mutex / Channel enum payloads, #125
nested affine struct fields + recursive Drop, #126 mixed-
place leaf-OwnedStr / leaf-Vec assign (F2) — Copy gate on
the leaf segment relaxed for heap-shaped types; both
backends emit a free of the old slot before storing the
new value. #127 Vec element drops walk owning fields:
`intent_vec_<S>__free` now iterates each element and drops
its owning resources for `S = OwnedStr`, `S = Vec<U>`, and
`S = Struct{…}` with owning fields, before freeing the
outer buffer. Closes a pre-existing leak in
`Vec<Struct{OwnedStr…}>` and `Vec<OwnedStr>` at scope
exit. Verified clean under `-fsanitize=address,leak`.
#128 OwnedStr enum payload destructure (D3): match arms
`Msg.Text(s) then …` admit OwnedStr-payload bindings; the
binding is exposed to the arm body as `Str` (Copy
borrowed-view), the scrutinee keeps ownership, and its
scope-exit Drop frees the heap exactly once. Vec / other
non-Copy payload bindings still rejected. #129 Block
expressions admit `print` stmts in the Let-prefix:
`{ let a = …; print "log", a; tail }` is now legal so
intermediate values can be logged inside block-expression
initializers (parser, checker, tree-C, tree-LLVM all
extended; SSA Block routing unchanged). #130 `try` desugar
admits intermediate `print` stmts and fixes a pre-existing
C codegen bug for payloaded-enum match results: the
`let v = try opt; …; return X;` desugar's intermediate
stmts can now include `print` (was Let-only). The C match
emitter was also using `c_element_storage` for the match
result type, which collapsed every enum to `int32_t` and
mismatched the arm bodies' struct literals; switched to
`c_type_name` so payloaded enums render as `Enum_<Name>`.
#131 Cross-backend parity runner covers all examples:
the runner was missing 14 of the 57 example programs (the
gap that allowed #130's C-side bug to ship undetected).
All 57 examples now run identically on both backends.
#132 FieldAssign with heap-shaped field frees old slot:
`t.name = newstr` (OwnedStr field) and `b.items = newvec`
(Vec field) now emit a free of the previous slot's heap
before storing the new value in both backends. Mirrors
closure #126's leaf-Drop logic for index-field assigns.
#133 Reassign of OwnedStr frees previous heap + LLVM
Reassign eval-order fix: `s = "b" + ""` no longer leaks
the previous heap (the Reassign drop-old path was Vec-only
in both backends; OwnedStr fell through to plain assign).
The LLVM Reassign emit also had a latent bug — it freed
the old buffer BEFORE evaluating the RHS, which would UAF
on any RHS that READ the binding; reordered to
eval-first-then-free. #134 `let _ = …` discard of
OwnedStr frees heap: `let _ = make_owned_str();` (and
bare `make();`) was silently leaking — tree-C, tree-LLVM,
and SSA Discard handlers all only knew about Vec. All
three now free the heap. SSA Reassign also extended to
lower `drop_old` for OwnedStr / Vec (was a hard reject).
#135 `print` of fresh OwnedStr frees heap: SSA / tree-C /
tree-LLVM print emit handlers now free OwnedStr printed
from a Call or Binary `+` expression (the v1
heap-producers); Var / FieldAccess / TupleAccess and
other variants stay non-consuming so the binding's
scope-exit Drop still owns the free (avoids double-free
on patterns like `print t.name`). #136 `Vec<OwnedStr>`
compiles to valid C: the C backend's `element_tag`
helper was leaking the `*` from `char*` into the typedef
name (`intent_vec_char*`), making the emitted C fail at
cc. Added explicit `str` / `owned_str` arms. LLVM was
already sanitizing. #137 `match make_owned_str() { … }`
drops temp scrutinee: fresh OwnedStr match scrutinees
(Call / `+` concat) were leaking — `check_match_str`
bound to a temp but never emitted a Drop. Restructured
the synthetic Block to wrap the if-chain through a
`__match_str_result_<n>` let, drop the temp, then yield
the result var. Tree-C / tree-LLVM Block codegen also
extended to emit Drop stmts. Var / FieldAccess
scrutinees stay unwrapped (would double-free with the
outer binding's existing Drop). #138 `strcmp` of fresh
OwnedStr drops heap: `make_owned_str() == "literal"`
(and other comparison ops) was leaking — strcmp doesn't
consume its arguments, so a fresh Call / Binary `+`
OwnedStr operand had no other owner. Fixed both the
SSA lowering and the tree-LLVM strcmp branch. Var /
FieldAccess operands skip the drop (existing
whitelist). #139 `len` of fresh OwnedStr drops heap:
`len(make_owned_str())` had the same shape — strlen
doesn't consume its argument; fresh Call / Binary `+`
operands had no other owner. Same whitelist-based fix
applied to the SSA `Len` lowering and tree-LLVM `Len`
emit. #140 unified `is_fresh_owned_str` helper +
tree-C strcmp/strlen fixes: the per-site whitelist
across #135 / #137 / #138 / #139 is now a single
`crate::ir::is_fresh_owned_str` helper that also
broadens the set to include Block / IfExpr / Match
(closes a `len({…})` leak that #139's narrower
whitelist missed). The tree-C `emit_len` and
`emit_binary` strcmp paths — previously untouched —
also now free fresh operands via GCC statement-
expression temps. #141 `len` of fresh Vec drops
buffer: generalized to `is_fresh_non_copy` (matches
OwnedStr + Vec<T>); SSA + tree-C Len for Vec now
free a fresh-Vec operand after reading `.len`.
Verified against a 1000-iter loop (was ~40KB
leaked). #142 `Index` of fresh Vec drops buffer:
`vec(1, 2, 3)[0]` was leaking the same way as
`len(vec(...))`. SSA + tree-C Index for Vec now
free a fresh-Vec operand after reading `.data[i]`.
#143 `clone(fresh_vec)` drops borrowed arg: the
checker treats `clone(xs)` as borrow-semantics; for
a fresh-Vec arg the buffer had no other owner. SSA
Call lowering now emits a Drop after the clone for
each fresh non-Copy argument. #144 intent_str_concat
l_owned flag for FieldAccess operand: was double-
freeing `t.name + "-suffix"` — the concat flag set
l_owned=1 for any OwnedStr-typed operand, freeing the
field's heap inside concat AND at the struct's per-
field Drop. Refined helper
`owned_str_consumed_at_concat`: l_owned=1 only for
Var (moved by op) or fresh (Call/Binary/etc.). #145
`let _ = make_struct()` frees heap fields: discard of
a struct with OwnedStr/Vec/nested-struct fields was
silently leaking — Discard handlers only matched
OwnedStr/Vec. Tree-C, tree-LLVM, and SSA Discard
extended to also drop Struct values (spill + per-field
walk). Tree-LLVM arm also reordered ahead of
`is_scalar` (which treats structs as scalars). #146
`let _ = make_enum()` frees heap payload: same shape
for enums with OwnedStr / Vec<T> payloads. Tree-C
spills to `Enum_<Name> _intent_discard` and switches
on the tag. Tree-LLVM mirrors the scope-exit Drop
logic for enums (extract tag/payload, OR-chain of
icmp eq, conditional free branch). SSA Discard emits
Drop for non-Copy enums. #147 Reassign of
Struct / Enum with heap fields: `t = Tag { name: … }`
and `m = Msg.Text(…)` were leaking the previous heap.
Tree-C Reassign now spills to a tmp, walks the OLD
binding's per-field drops (Struct) or switches on tag
to free payload (Enum), then moves the tmp in. SSA's
drop_old whitelist extended to admit non-Copy Struct
/ Enum; LLVM is covered by the existing Drop emit
machinery. #148 FieldAssign of Struct-typed field frees
old heap: `o.inner = newInner` where Inner has heap
fields was leaking the previous Inner's heap.
FieldAssign's heap-overwrite logic (from #132) only
handled OwnedStr / Vec field types; Struct fell
through. Tree-C now walks the OLD struct field's
per-field drops via `emit_struct_field_drops`. #149
IndexAssign of Struct/Enum element frees old heap:
`xs[i] = newStruct` for a `Vec<Struct{heap-field}>`
was leaking the OLD element's heap fields. The
leaf-drop logic (closure #126) only fired when
field_path was non-empty; whole-element overwrites
fell through. Tree-C now also handles
`field_path == []` for leaf-Struct (per-field drop
walk) and leaf-Enum (tag-switch payload free).
#150 IndexAssign whole-element for OwnedStr/Vec
elements: same shape — closure #149 only added
Struct/Enum arms; `Vec<OwnedStr>[i] = …` and
`Vec<Vec<i64>>[i] = …` fell through. Tree-C and
SSA-C now free the OLD slot for OwnedStr / Vec
element types in the whole-element overwrite case.
#151 `Vec<PayloadedEnum>` compiles + drops
correctly: was broken in four places — C
`element_tag`/`c_element_storage` collapsed enum
elements to `int32_t` (cc rejected struct literals
into i32 slots); `c_element_drop_old` lacked an
Enum arm (per-element drop body empty, payloads
leaked); LLVM vec literal used `vec_element_byte_size`
returning 8 for enums (under-allocated 16-byte
tagged union, crashing lli with invalid free).
All four sites now treat payloaded enums like
structs/tuples. #152 `clone(Vec<OwnedStr>)` /
`clone(Vec<Enum>)` deep-copies: the per-shape Vec
__clone was shallow-copying per-element heap
pointers, double-freeing at scope exit. C
`c_element_deep_clone` extended for OwnedStr (via
`intent_str_concat(slot, 0, "", 0)`) and Enum (tag-
switched ternary reconstructing with deep-cloned
payload). LLVM's Vec __clone loops over slots for
ANY non-Copy element type and produces per-element
deep clones (was only handling Vec<U>; OwnedStr /
Enum payloads fell through to an uninitialized
buffer that crashed lli). #153 same shape for
`clone(Vec<Struct{heap-field}>)` — adds the Struct
arm to both backends' deep-clone paths. #154
`clone_at(ref xs, i)` for OwnedStr was broken: SSA-C
had no handler (fell through to undefined
`fn_clone_at`); tree-LLVM panicked with "not yet
supported". Both backends now do per-element deep
clones (SSA-C via `c_element_deep_clone`, tree-LLVM
inline via `intent_str_concat`). #155 `clone_at`
Struct element: tree-LLVM also panicked for Struct
element types; now extracts each field and deep-
clones OwnedStr fields. Tree-C was already correct
via `c_element_deep_clone`'s recursive Struct arm
from #153. #156 `clone_at` Enum element: same shape
— tree-LLVM was panicking for Enum elements. Now
OR-chain-checks the tag against payloaded tags,
branches to a deep-clone path
(`intent_str_concat` of the OwnedStr payload +
insertvalue into a new enum struct) vs a tag-only
path (pass slot through), phi-joins. #157 LLVM
Vec `__set` frees old element: the per-shape Vec
__set helper only freed for `Type::Vec(inner)`
elements; `set(Vec<OwnedStr|Struct|Enum>, …)`
leaked the previous slot's heap. Closure #127 had
extended tree-C; #157 closes LLVM with OwnedStr /
Struct / Enum arms. #158 SSA-LLVM vec set/push/clone
arg type fix: `emit_vec_call` was falling back to
`element.clone()` for any `Operand::Const` (since
`operand_type` returns None for Const), typing
`set`'s i64 index as the element type (`i8* 0` for
`Vec<OwnedStr>`). Per-builtin signature lookup
(`sig_at(pos)`) now returns `i64` for `set`'s
middle arg, `Vec<T>` for the first arg, and the
element type only for the value slot. #159 Consuming
`for x in xs` over `Vec<non-Copy>` shallow-frees: the
post-loop code called `intent_vec_<T>__free(xs)` which
(since closure #127) walks every slot and re-frees
each element's heap — double-freeing what `x`'s
scope-exit drop already released per iteration. Tree-C
and tree-LLVM `emit_for_iter` now emit a shallow
`free(xs.data)` for non-Copy element collections;
Copy-element collections still route through the
deep `__free`. SSA path (which never emitted any
post-loop drop, silently leaking the outer buffer)
is gated out: `stmt_ssa_supported` rejects consuming
for-iter over non-Copy Vec, falling back to tree.
Verified clean under `-fsanitize=address,leak` for
`Vec<OwnedStr>`, `Vec<Vec<i64>>`, and
`Vec<Struct{OwnedStr,i64}>`. #160 tree-LLVM Block-
expression emits Drop stmts: `check_match_str`
desugars `match <fresh OwnedStr> { … }` into Block
{ Let temp = scr; Let result = ifchain; Drop temp;
result } (closure #137), but tree-LLVM's Block
emitter only forwarded `Let` and `Print` to
`emit_stmt` — the Drop was silently discarded,
leaking the scrutinee's heap on every match call.
Tree-C already handled Drop in its Block emitter.
Now tree-LLVM forwards Drop too. #161 tree-LLVM
`len(ref Vec)` was returning 0: the Len emitter only
matched `array.kind == Var(name)`. `len(ref xs)` has
`array.kind = Ref { name }` so it fell through to a
fallback that emits the static-length value (0 for
Vec). Now Ref / RefMut(name) take the same alloca
address as Var(name) and route through the GEP-into-
.len + load path. #162 closes the other two
field-shape spellings: `len(ref t.items)` /
`len(mut ref t.items)` (RefField/RefMutField) and
`len(t.items)` (FieldAccess yielding a Vec). Both
fell through to the static-length fallback (i64 0)
that the lli verifier rejected outright. Field-
borrow forms reuse the field-pointer emit_expr
materializes; FieldAccess goes through
emit_lvalue_addr. Both then GEP-into-.len + load.
#163 tree-LLVM Index for Vec-typed struct fields:
`b.items[1]` panicked with
`unreachable!("Index on unsupported base")`. The
FieldAccess arm only handled Array-typed fields;
the parallel Vec arm now reuses emit_lvalue_addr
+ .data GEP + load + element GEP + load. #164
tree-C struct typedefs topologically sorted by
direct field dependency (Struct field or Array
element). Source-order emit was producing
`Struct_Outer { Struct_Inner inner; }` before
`Struct_Inner` was declared, which cc rejected.
LLVM's IR forward-declares named types so tree-LLVM
was unaffected. #165 RefField/RefMutField now carry
the binding's `object_ty`. `ref self.items` inside
a `self: ref T` method body was emitting
`&v_self.items` (cc: invalid member access) in tree-C
and `getelementptr %Struct_T*, %Struct_T** %arg_self`
(lli: indirection mismatch) in tree-LLVM. Tree-C
picks `.` vs `->` from `object_ty.is_any_ref()`;
tree-LLVM derefs `object_ty` before spelling the GEP
source type. #166 FieldAssign marks RHS Var moved:
`self.name = n;` was double-freeing the new heap —
the parameter binding's scope-exit drop fired even
though the field had taken ownership. The Let /
Reassign / Call-arg arms already called
`consume_if_moved_var` for non-Copy RHS Vars;
FieldAssign was missing that call. One-line
addition. #167 tree-LLVM `xs[i] = v` was leaking
the old slot on `Vec<OwnedStr>` / `Vec<Vec<T>>`:
`emit_leaf_overwrite_drop` early-returned when
`field_path.is_empty()`, skipping the bare-leaf
case. The OwnedStr / Vec arms work for both deep
and bare paths since `p` is the slot pointer in
either case. Removing the guard fixes the leak;
Copy element types stay no-ops via the wildcard
arm. #168 tree-LLVM `let _ = s` where s: OwnedStr
was leaking: the Discard handler's OwnedStr arm sat
AFTER `is_scalar(&expr.ty)` but `is_scalar` returns
true for OwnedStr — the scalar arm consumed the
branch and skipped the @free. Moving the OwnedStr
arm before the is_scalar guard fixes it (same shape
as closure #145 for Struct). #169 tree-LLVM Reassign
drop_old extended to Struct and Enum: bindings of
heap-owning structs and payloaded enums were leaking
the OLD value's heap on reassign. Tree-C had the
parallel arms via closure #147; tree-LLVM only had
Vec / OwnedStr. Added the Struct arm
(emit_llvm_struct_field_drops over the old alloca)
and the Enum arm (load + tag-branch + free payload —
mirrors the Drop handler). #170 tree-LLVM nested
FieldAssign drop_old extended to Struct and Enum:
`o.inner = NewInner { … };` was leaking the OLD
nested struct's heap fields. Tree-C had the
parallel arms via closure #148; tree-LLVM only had
OwnedStr/Vec. Added Struct arm (walks the OLD
field's heap-owning sub-fields via
emit_llvm_struct_field_drops at the field pointer)
and a defensive Enum arm matching the Reassign
shape. #171 `push(xs, v)` / `set(xs, i, v)` consume
the VALUE Var when it owns non-Copy heap. Builtin
handlers were calling consume_if_moved_var on
args[0] (the Vec) but not on args[1]/args[2] (the
value), so the source Var's scope-exit drop double-
freed the heap now owned by the new Vec's slot.
ASan caught it on chained pushes; both backends
were affected since it's a checker/IR-level bug.
Two-line fix. #172 If-expr non-Copy Var branches
were double-freeing: `let chosen = if cond { a }
else { b };` (a, b: OwnedStr Vars) emits a ternary
`cond ? v_a : v_b` so v_chosen aliases one Var's
heap — scope-exit drops of v_a, v_b, and v_chosen
all hit the same heap. `consume_if_moved_var` now
recurses into IfExpr branches, marking each
branch's Var moved (conservative). Both backends
were affected. Known limitation: the unchosen
alternative leaks (no double-free, just lost
heap); structural rewrite needed to free it inside
each branch. Test totals: 864 lib + 47 e2e
passing.

### Closed: if-expr / match Var-branch unchosen leak (#179)

The conservative-move shape from #172/#173 used to
leave the unchosen alternative's heap unreclaimed.
Closure #179 rewrites the typed expr's branches via
`inject_branch_drops` so each branch wraps its chosen
value in a Block that drops the OTHER branches' Var
leaves before yielding — closing the leak without
introducing double-frees.

#173 Match arms returning Var of non-Copy were
double-freeing in the same way as #172. The
integer / enum / bool match keeps `TypedExprKind::
Match` shape (the Str scrutinee desugars to an
IfExpr chain via check_match_str so it's covered
through #172). `consume_if_moved_var` now recurses
into every arm's body the same way it descends into
both if-expr branches. Same known limitation:
unchosen-arm Vars leak; structural rewrite needed.
#174 Block-expr tail Var: `let b = { …; a };` was
double-freeing because the tail Var aliased into b
without `a` being marked moved. consume_if_moved_var
now descends into Block { tail }. Same family as
#172/#173. #175 SSA-C `OwnedStr` declared `char*`
(was `const char*`, same as Str). Vec helper
bundle's `char* data` field rejected const stores
with -Wdiscarded-qualifiers; warning hid real
diagnostics. #176 SSA-C `ref Channel<T,N>` param
drops `const`: send/recv helpers take non-const
pointers (atomically mutate seq counters + idx);
the const-qualified ref parameter raised
-Wdiscarded-qualifiers on every site. Mirrors the
Atomic-ref arm. #177 `vec(a, b, …)` consumes each
Var element when non-Copy. check_vec_builtin
forgot to call consume_if_moved_var on its element
args — source Var's scope-exit drop fired after
vec() already moved the pointer into the slot, then
Vec's __free re-freed each slot. Same family as
push / set (#171). One-line addition. Test totals:
869 lib + 47 e2e passing. #178 `Enum.Some(v)`
consumes Var payload arg: enum-constructor was
storing the payload pointer into the tagged-union
without marking the source Var moved, so scope-exit
drop double-freed against the enum's payload drop.
One-line addition (same family as #171, #177).
Test totals: 870 lib + 47 e2e passing. #179
inject_branch_drops: structural rewrite of if-expr /
match / block-tail typed expressions so each branch
wraps its chosen value in a Block that drops the
OTHER branches' Var leaves before yielding. Closes
the unchosen-alternative leak the conservative move
tracking in #172/#173 left behind. Wired into Let,
Reassign, IndexAssign, FieldAssign. Recursive
walker handles nested if-expr / match / block. Test
totals: 871 lib + 47 e2e passing. #180 extends the
inject to the remaining consume sites: named Call
args, MethodCall args, StructLit field values,
EnumVariantWithPayload arg, vec elements, push and
set values. `f(if cond { a } else { b })` and
similar shapes no longer leak the unchosen
alternative. Test totals: 872 lib + 47 e2e passing.
#181 inject_branch_drops also at the Return-stmt
arm: `return if cond { a } else { b };` was leaking
the unchosen Var since the inject wasn't wired into
Return. One-line addition. Test totals: 873 lib +
47 e2e passing. #182 push / set xs arg also: the
builtin handlers had wired inject_branch_drops into
the value arg (#180) but not the Vec arg. Symmetric
fix. Test totals: 874 lib + 47 e2e passing.
#183 `is_fresh_owned_str` / `is_fresh_non_copy`
refined to recurse into if-expr / match / block-tail
branches: only treat as fresh when every leaf is
itself a fresh producer (Call / Binary). Var leaves
disqualify. Closes a print-of-if-expr-Var-branches
double-free. Test totals: 875 lib + 47 e2e passing.
#184 SSA consuming for-iter: lower_for_iter ignored
the consumes flag, leaking the outer Vec buffer on
normal completion. SSA gate routes Vec<non-Copy>
consume to tree backends (#159), so SSA only sees
Vec<Copy>; intent_vec_<T>__free is shallow for Copy.
Emit InstrKind::Drop for the consumed Vec at the
exit block. Known remaining: early `return` from
inside the body still skips this Drop (documented
known limitation). Test totals: 876 lib + 47 e2e
passing. #185 SSA for-iter continue infinite loop:
`continue` jumped straight to the header with the
OLD i_header — increment only fired on natural
body-end fallthrough. Pre-existing bug since SSA
for-iter shipped. Restructured with a `step` block
that takes the carry params, increments, then
jumps to header. Continue and natural-end both
jump to step uniformly. Test totals: 877 lib + 47
e2e passing. #186 same bug fixed in tree-LLVM: it
had the same continue-infinite-loop pattern. Same
iter_step block fix. Tree-C uses C's native
`for (i = 0; i < len; i++)` form so it was
unaffected. Test totals: 878 lib + 47 e2e passing.
#187 SSA range-for continue had the same bug as
#185 for the for-iter form. `for i from a to b` in
SSA's lower_integer_for now uses the same step-block
shape. ParallelForShape grew a `step_block` field;
SSA-C / SSA-LLVM parallel-for emit skip step
alongside header/body. Test totals: 879 lib + 47
e2e passing. #188 same continue-infinite-loop bug
fixed in tree-LLVM `TypedStmt::For` (range form).
Same iter_step / for_step shape. Test totals: 880
lib + 47 e2e passing. #189 tree-LLVM outlined
parallel-for fn didn't push a LoopFrame, so
`continue` inside the parallel-for body fell
through to the "outside a loop" path → wrong
reduction total. Fix: LoopFrame{header:step,
exit:exit} + step block doing load/+1/store/jump-
to-hdr. Closes the family of for-loop continue
bugs. Test totals: 881 lib + 47 e2e passing.
#190 parallel-for body rejects `break`: OpenMP
forbids break inside `omp parallel for`, but the
checker silently allowed it. C backend then
generated `break;` inside the pragma which gcc/clang
refused to compile. Checker now diagnoses with a
clear message pointing at the Mutex<bool> workaround.
Test totals: 882 lib + 47 e2e passing.
#191 task body_blocks calculation used a contiguous
ID range; closures #185/#187 step blocks plus
if-then/else/merge blocks in task body got BlockIds
beyond end_block → fell outside range → parent
emitted them with goto-targets to skipped blocks.
Fix: CFG-reachability walk from begin to end.
Test totals: 883 lib + 47 e2e passing.
#192 tree-C Block-expr Drop Struct emit: the
inject_branch_drops machinery wraps if-expr / match
arms with Drops for the OTHER branches' Vars; for
Struct Vars the Block emit's Drop arm fell through
`_ => {}` and the unchosen branch's heap leaked.
Added the Struct arm that walks
STRUCT_FIELDS_REGISTRY and emits the per-field free
chain. Test totals: 884 lib + 47 e2e passing.
#193 same shape for Enum: tree-C Block-expr Drop
arm now emits `switch (v.tag) { case T: free; break;
default: break; }` for payloaded enums. Mirrors the
Reassign Enum drop (#147). Test totals: 885 lib +
47 e2e passing.
#194 Block-expression sibling-let leak: `let r = {
let a = …; let b = …; a };` leaked b. The Block-expr
check pushed and popped a scope but never emitted
scope-exit Drops. Fix in `check_expr` for
`ExprKind::Block`: call `consume_if_moved_var(tail,
…)` to propagate tail-Var moves into the inner scope,
then push Drops for non-moved non-Copy bindings.
When drops exist, spill the tail into a synthetic
`__block_tail_<span>` Let so the Drops fire AFTER
the tail evaluates (avoids UAF for tails that borrow
a sibling, e.g. `{ let a = …; len(a) }`). When the
tail consumes every sibling (binary concat, fn args)
the drops list is empty and no spill is emitted.
Tree-C and tree-LLVM both benefit — Block emit was
already wired for Drop stmts (#160, #192, #193).
Test totals: 887 lib + 47 e2e passing.
#198 tree-C tuple-shape collection in control flow:
`collect_tuple_shapes_in_expr` had Tuple/TupleAccess/
Unary/Binary/Call/ArrayLit/Cast/Index/Len/CallIndirect
arms but fell through `_ => {}` for Block/IfExpr/Match.
A tuple type that only appeared inside a Block-expr
inner Let never had its `intent_tuple_<…>` typedef
emitted; cc rejected. Vec walker already handled
Block/IfExpr/Match (#129). Mirrored the same three arms.
Test totals: 891 lib + 47 e2e passing.
#197 Block-expr inner type-alias resolution: parallel
to #196 for the type-alias substitution pass.
`sub_aliases_in_stmt` had the same pre-existing
limitation — never descended into a Stmt's `expr` field,
so `let p: AliasName = …;` inside a Block-expr kept the
unresolved alias. New `sub_aliases_in_expr` walks every
expression shape and recurses through nested Lets,
mirroring the #196 enum walker. Test totals: 890 lib +
47 e2e passing.
#196 Block-expr inner enum-let annotation resolution:
`resolve_enum_types_in_stmt` walked top-level fn bodies
and `if`/`while`/`for`/`for-iter`/task bodies but never
descended into a Stmt's `expr` field, so any Let inside
a Block-expr (e.g. `let r = { let a: Maybe = …; … }`)
kept its annotation as `Type::Struct("Maybe")`. The
identical-text "Maybe vs Maybe" diagnostic surfaced when
the variant constructor's `Type::Enum` didn't match the
unresolved annotation. Fix: new
`resolve_enum_types_in_expr` walks every expression
shape (Block, IfExpr, Match, Cast, Binary, Call, Tuple,
StructLit, FieldAccess, Try, …) and recurses through
nested Lets. Pre-existing bug exposed by writing
enum-typed inner Lets. Test totals: 889 lib + 47 e2e
passing.
#195 inject_branch_drops cross-scope leak after #194:
the spill from #194 lands `let __block_tail_<span> =
…` inside each Block-expr, and `collect_branch_var_
leaves` was treating the inner spill Var as a "leaf"
of the surrounding if-expr branch. inject_branch_drops
then emitted `Drop __block_tail_<span>` in the OTHER
branch where the name isn't declared → cc rejected
with `undeclared identifier`. Fix: when descending
into `Block { stmts, tail }`, filter out any Var name
that a `Let` inside the same Block introduces. Same
filter helps user-declared inner Vars too. Test
totals: 888 lib + 47 e2e passing.

### Recommended next (pick one)

- **A. Dynamic dispatch (vtables) — completes #7 Phase 2.**
  Bounded generics shipped. Remaining piece is first-class
  interface objects.
  Concrete entry points:
    - Add a `Type::Object(iface_name)` variant for first-class
      interface objects; backends emit a `{ &vtable, &data }` fat
      pointer.
    - Auto-`==` for structs/tuples/enums by lowering `a == b` to
      `a.eq(ref b)` when an `implement Eq for T` is in scope.
  Effort: medium/high.

- **B. #3 polish — partial-move tracking + Vec field methods.**
  Field-borrow shipped (#102), so `atomic_*(ref c.hits)` works.
  Remaining gaps:
    - **Partial-move tracking**: `let y = t.xs;` moves the whole
      struct; we want it to move only the field and leave the
      rest valid. Per-field `moved` map on `BindingInfo`.
      Unlocks `push(mut ref t.xs)` (currently rejected because
      Vec push takes Vec by value, and field-borrow gives
      `mut ref Vec<T>` which doesn't match).
    - **Multi-field drop order**: reverse-declaration order
      (Rust convention) — today the field list is walked in
      declaration order.
    - **Mutex / Guard / Channel struct fields**: bespoke RAII
      shape; need per-backend Drop dispatch.
  Effort: medium per item; can interleave.

- **C. Drop for structs with heap fields.**
  Today the auto-call is suppressed for structs with OwnedStr /
  Vec fields (per-field free runs instead). Two designs would
  unblock the combined case:
    - Change Drop signature to `fn drop(mut self: T)` so the user
      can mutate fields before the per-field free runs.
    - Or run user Drop FIRST (still consuming self), then
      synthesise a separate field-free pass that operates on a
      shadow copy. Requires the affine system to model
      "consumed but field-resources still owned".
  Effort: medium (design call first).

### Other queued follow-ups (smaller, can interleave)

- **#8 Phase 2: Drop auto-call at scope exit** — wire `T_drop` into
  the existing `TypedStmt::Drop` lowering so user-declared
  `implement Drop for T` runs automatically. Blocked on B above
  for nested affine fields.
- **#5 follow-ups**: `try` in nested blocks, non-let statements
  between `try` and `return`, multiple `try`s in one block. Each
  is a small AST-level extension of `desugar_try_let_in_program`.
- **Devanagari (parked at user's request)**: script-aware
  diagnostics, multi-word alias expansion, grammar-consultant
  review of the Sanskrit / Hindi / Marathi tables.

### README "Todo (small)" — most land naturally with A or B

- `const N` as `[T; N]` length (const-eval pass)
- Const initializer arithmetic (`const B: i64 = A + 1`)
- Array types in fn return position (SSA gap)
- Nested arrays `[[T;N]; M]`, `[Vec<T>; N]` (SSA gap)
- Empty struct `struct E {}` (parser tweak)
- Unit-return fns (`fn f() { … }` no `->`)
- Type-associated functions `Type.helper()`
- `bool ↔ int` cast (deliberate; may stay deferred)
- SSA bool-print renders `1`/`0` not `true`/`false`
- Bare `{ … }` as scope-stmt
- `xs[i].field = v` mixed-place assign
- Struct/tuple/enum `==` (lands with #7 Phase 2 → option A)
- Match on `bool` / `Str` / `f64` scrutinees

### Deferred (multi-week)

- Cranelift backend
- Direct-asm targets (x86_64-linux first)

## Foundational items — dependency-ordered queue (2026-05-21)

English-language core foundations come first. Devanagari work
(#83–#87) parked until the foundational queue is closed.

Critical-path analysis of what's left in the multi-session queue:

```
#4  T1.3 phase 2b: tagged-union codegen + pattern bindings
    └─ no deps
    └─ unlocks #5
    └─ effort: high (multi-session)

#5  T2.6: Option<T> / Result<T,E> + try keyword
    └─ depends on: #4
    └─ unlocks: idiomatic error handling
    └─ effort: low/medium

#3  T1.2 phase 2b: RAII + non-Copy struct fields
    └─ MVP DONE 2026-05-21 (closure #98)
        OwnedStr struct field works on both backends;
        each owning field is freed at scope exit; struct-literal
        init from a Var moves the source binding.
        Remaining: Vec / [T;N] / Task / Atomic fields,
        multi-field drop order, partial-move tracking.

#6  T1.4 phase 2: generic call-site monomorphization
    └─ no deps
    └─ unlocks #7
    └─ effort: high

#7  T1.5 phase 2: interface dispatch + bounded generics
    └─ depends on: #6
    └─ unlocks #8, user-defined ==
    └─ effort: medium/high

#8  T2.7: user-defined Drop interface
    └─ depends on: #7
    └─ unlocks: RAII for user types
    └─ effort: low/medium
```

Critical paths:

  #4 → #5                  (2-deep, error-handling)
  #6 → #7 → #8             (3-deep, generics/interfaces)
  #3                        (standalone, struct RAII)

All items #1-#9 above have at least a Phase 1 / MVP shipping
as of 2026-05-21. Remaining work is follow-up phases tracked
inline (e.g. T2.7 Phase 2 auto-drop, T1.2 phase 2b non-Copy
aggregate fields, T1.5 phase 2 dynamic dispatch).

Devanagari follow-ups (parked, low priority until above is closed):

  D1. Script-aware diagnostics (medium)
  D2. Multi-word alias expansion / consultant review

## Pending — canonical order (2026-05-19)

Resume from this section after a session break. The list mixes
in-progress and queued items in priority order, smallest-with-
highest-leverage first.

1. ~~**`A3` follow-up: SSA lowering of fn-pointers**~~ —
   done 2026-05-19. New `InstrKind::FnRef { name }` and
   `InstrKind::CallIndirect { callee, args }` carry through
   the SSA layer; the lowerer emits them in
   `lower_expr_to_operand` (replacing the previous
   `LowerError`). All SSA passes were updated: substitution
   walks the operand list, DCE treats `FnRef` as pure-no-trap
   while keeping `CallIndirect` alive, and the
   `audit_pure_regions` effects gate now emits a new
   `PureViolationKind::IndirectCall` for any `CallIndirect`
   inside a hint-marked region. The `ssa_backend_c.rs`
   scalar backend emits `v_<n> = fn_<name>;` for FnRef and
   `v_<n> = <callee>(args);` for CallIndirect. The skip in
   `tests/ssa_examples.rs` was removed —
   `examples/fn_pointers.intent` now flows through the SSA
   lowerer cleanly. 1 new lib test pins: a synthetic
   `ParallelForBegin`/`End` pair around an indirect call
   surfaces the `IndirectCall` violation kind.
2. ~~**`5` Real-threading for `task`**~~ — done 2026-05-19.
   See entry above. The spawn lowers to pthread on both
   backends, captures are Copy-only, join blocks via
   pthread_join and frees the ctx.
3. **`6g` SSA-C aggregates + parallel-for + tasks** —
   partial as of 2026-05-19, multi-session remaining.
   Done: read-only arrays (`ArrayLit`, `Index`, `Len`) via
   a declarator-style emit (`int64_t v_0[N]`); string
   literals (`StrLit` → escaped C string constant);
   `Drop` + `Hint` markers pass through as no-ops. The
   SSA-C cross-check test grew three array programs that
   compile through and exit with the expected code.
   Done 2026-05-19: 3a — IR refactor for `IndexAssign` /
   `RefOf` / `Drop`. `array_name: String` and `name: String`
   were replaced with an `Operand` ("the SSA value the
   binding currently holds"), populated by the lowerer via
   the existing `Locals` map. The audit pass / SMT bounds
   elision / DCE were updated. The SSA-C backend now lowers
   IndexAssign directly via `c_operand(array)[idx] = value`
   and RefOf as `&v_<id>` (or array decay for `&[T; N]`).
   The cross-check test grew an `array_index_assign`
   program that compiles + exits 99. `Drop` also gained
   the `source: Operand` field alongside the existing
   `name` (which the drop-coverage audit still uses for
   reporting).
   Done 2026-05-19: 3b — `Vec<T>` lowering in SSA-C. The
   tree-C backend's `emit_vec_bundle`, `vec_c_struct`,
   `vec_helper`, `element_tag` were promoted to
   `pub(crate)` and the SSA-C backend now walks the SSA
   module for `Type::Vec(_)` element types, emits one
   runtime bundle per element, and routes
   `vec()`/`push`/`set`/`clone` Call instructions through
   the matching `intent_vec_<T>__<op>` helper. The SSA-C
   `c_declarator` got Vec arms (owned + ref/refmut). A
   per-function `ValueId → Type` map (built during forward
   declarations) drives the new Index/Len/IndexAssign
   dispatch: Vec operands deref through `.data` / `.len`
   (or `->data` / `->len` for refs), arrays use the
   pointer-decay form. Drop of Vec calls
   `intent_vec_<T>__free`. Preamble grew `<assert.h>` +
   `<string.h>` + the `INTENT_UNUSED` macro so the shared
   helper bundles compile. New cross-check program
   `vec_creates_and_indexes` (`vec(10,20,30)[1] = 20`).
   Done 2026-05-19: 3c + 3d shipped as sequential
   lowering. The SSA already emits parallel-for as a
   regular for-loop with `Hint::ParallelForBegin/End`
   markers; tasks lower the body inline between
   `Hint::TaskBegin/End/Join` markers. SSA-C treats every
   hint as a no-op so the body executes sequentially —
   semantics-preserving because the verifier already
   proved each parallel-for iteration is independent and
   each task body is race-free with respect to
   captures. Two new cross-check programs verify the
   path: a task spawns + joins around a Copy capture
   (exit 0); a parallel-for sums `xs[i]` into a
   reduction var (exit 10). Real pthread / libgomp
   parallelism via SSA-C is a separate follow-up —
   listed below as the "parallel SSA-C lowering" gap.
   Remaining sub-items (each separately landable):
   - **Parallel SSA-C/SSA-LLVM lowering** — *steps 1 + 2a
     landed 2026-05-19*. Status of each step below.
       - **Step 1: ParallelForShape on Hint** — done.
         `HintKind::ParallelForBegin` carries the structured
         loop shape; `lower_integer_for` returns an
         `IntegerForShape` that the parallel-for arm patches
         into the begin-hint via a placeholder-then-patch
         flow.
       - **Step 2: SSA-C parallel-for emit + gate drop** —
         done. SSA-C now handles `parallel for` end-to-end:
         `collect_parallel_regions` + `recognize_parallel_region`
         + `emit_parallel_for_region` cover single-block
         bodies with the canonical {counter, …reductions}
         carry shape; emit produces structured for-loop +
         `_Pragma("omp parallel for [reduction(op: v_<carry>)…]")`
         + reduction rebind + exit-block transition.
         Multi-block bodies and non-canonical carry shapes
         surface `EmitError` → tree-C fallback. `min`/`max`
         recognized as intrinsics in SSA-C call emit (inline
         ternary, matches tree-C). The existing
         `emit_c_parallel_for_pragma_appears_in_output` test
         was relaxed to accept either tree-C's source-binding
         spelling (`v_total`) or SSA-C's SSA-id spelling
         (`v_<id>`) — both are OpenMP-functionally
         equivalent. Gate's `parallel for` clause dropped for
         SSA-C. `parallel.intent` runs through SSA-C with
         correct outputs.
       - **Step 3: SSA-LLVM consumer (simple shape)** —
         done 2026-05-19. `collect_parallel_regions_llvm`
         pre-scans for `ParallelForBegin` regions (reuses
         SSA-C's `recognize_parallel_region`).
         `emit_parallel_for_region_llvm` emits the parent
         side: pre-header instructions, ctx-struct alloca
         `{ i64 start, i64 end }`, field stores, `call void
         @GOMP_parallel(void (i8*)* @__intent_par_<N>, i8*
         %ctx_raw, i32 0, i32 0)`, exit-block param
         materialization, branch to exit.
         `emit_outlined_parallel_for` writes the outlined
         function to a `DEFERRED_FUNCTIONS` thread-local
         buffer (spliced into the module output after main
         functions): loads start/end from ctx, computes the
         thread's iteration slice via
         `omp_get_thread_num`/`omp_get_num_threads` (ceil
         work-distribution chunk math), runs the body block
         with the SSA counter ValueId aliased to the local
         iter var. New SSA-LLVM declares for
         `@GOMP_parallel`/`@omp_get_thread_num`/`@omp_get_num_threads`.
         The shape recognizer rejects captures and
         reductions via `EmitError`; the fallback in
         `emit_llvm_via_ssa` switches to tree-LLVM. The
         gate's `parallel for` clause for SSA-LLVM is now
         gone — defense-in-depth lives in the recognizer.
       - **Step 3-extended: SSA-LLVM reductions + captures** —
         the simple-shape outlining handles only the
         no-captures, no-reductions case. To close: (a) walk
         the body block for free variables defined outside
         the region, marshal them through additional
         ctx-struct fields, emit corresponding loads in the
         outlined fn (mirror tree-LLVM's
         `collect_outer_captures` + ctx-struct synthesis);
         (b) for each reduction in `ParallelRegion.reductions`,
         allocate a per-reduction storage in the parent (or
         pass through ctx), emit `atomicrmw add`/
         `cmpxchg`/etc. in the outlined fn body per
         tree-LLVM's reduction op table (with i8 shadow for
         bool reductions). ≈ 1 session.
       - **Step 4: Tasks SSA-C/SSA-LLVM** — not started.
         Extend `Hint::TaskBegin` with a `TaskShape` struct
         (handle name, captures with types, body blocks);
         emit pthread/CreateThread outlining in both
         backends. ≈ 1 session.
       - **Step 5: drop the gate's parallel-for / task
         clauses** once backends gain coverage.
4. **`6g` SSA→LLVM backend** — partial as of 2026-05-19,
   multi-session remaining. New module
   [src/ssa_backend_llvm.rs](src/ssa_backend_llvm.rs):
   - Done: scalar / control-flow / arithmetic skeleton.
     Function emission with parameters, block params lowered
     to LLVM `phi` nodes via a CFG predecessor map (compute
     once per function, walk each block's incoming edges),
     full coverage of `InstrKind::Const` / `Unary` / `Binary`
     (arithmetic + comparisons + shifts + bitwise) /
     `Cast` (int↔int / int↔float / float↔float) / `Call`
     (direct), terminators (`ret`, `br`, conditional `br`,
     `unreachable`), `Hint` markers as no-ops. A new
     `tests/ssa_backend_llvm_crosscheck.rs` runs six curated
     programs end-to-end through `lli` and asserts exit
     codes match expectations. 3 unit tests pin the
     instruction-level shapes.
   Done 2026-05-19: 5b — arrays in SSA-LLVM. `ArrayLit`
   allocates `[N x T]` then stores each element via GEP;
   the SSA value carries `[N x T]*` (alloca pointer) so
   subsequent `Index` / `IndexAssign` lower to `getelementptr
   <array_ty>, <array_ty>* <ptr>, i64 0, <ty> <idx>` + `load`
   / `store`. `Len` materializes the static constant. `Drop`
   of `Type::Array` (and `Type::Task`) is a no-op — stack
   storage is freed at function return. Phi / function-
   signature / parameter emit routes through a new
   `llvm_type_string` helper so aggregate types stay on the
   array's `[N x T]*` SSA spelling. Three new
   `tests/ssa_backend_llvm_crosscheck.rs` programs run
   end-to-end via `lli`: `array_literal_index` returns 20,
   `array_index_assign` returns 99, `array_loop_sum`
   returns 10.
   Done 2026-05-19: 5d — fn-pointers in SSA-LLVM. `FnRef`
   emits a `bitcast … @fn_<name> to <fn-ptr-ty>` to
   materialize the function symbol as a typed SSA value.
   `CallIndirect` emits
   `call <ret> (<params>) %v_callee(<typed args>)` using
   `llvm_type_string` to spell the callee's `fn(T1, …) →
   R` type. Direct `Call` argument types now route
   through `llvm_type_string` too so fn-ptr args pass
   cleanly. One new `tests/ssa_backend_llvm_crosscheck.rs`
   program `fn_ptr_indirect_call` passes `double` through
   `apply(f, x)` and exits 42.
   Done 2026-05-19: 5e — `StrLit` and `RefOf` parity.
   `StrLit` emits a private `@.str.<n> = private constant
   [N x i8] c"…\00"` global via a thread-local
   `STR_GLOBALS` buffer spliced between the preamble and
   function defs, then GEPs into it for the `i8*` SSA
   value. `RefOf` checks the source operand's type: for
   already-pointer values (arrays / refs) it bitcasts to
   the declared result type; for scalar SSA values it
   materializes a snapshot alloca + store + bitcast so the
   ref points at a stack location. Tested via the
   existing fn-ptr cross-check (StrLit not yet directly
   exercised — first need a programmable `print` to
   observe).
   Done 2026-05-19: 5c — `Vec<T>` in SSA-LLVM. Walk the
   SSA module for `Type::Vec(T)` element types; emit the
   per-element `%intent_vec_<elt> = type { T*, i64, i64 }`
   typedef + the tree-LLVM `emit_vec_helpers` runtime
   (`__push`, `__set`, `__clone`) shared via `pub(crate)`.
   `vec(…)` lowers inline: malloc + per-element GEP+store
   + `insertvalue` chain (mirrors
   `emit_vec_let_from_literal` in the tree backend).
   `Index` / `Len` over Vec extract `.data` / `.len` and
   GEP-load. `Drop` of `Vec<T>` extracts `.data`, bitcasts
   to `i8*`, calls `@free`. The cross-check program
   `vec_creates_and_indexes` (`vec(10,20,30)[1] = 20`)
   runs end-to-end via `lli`. `@realloc` was added to the
   extern declarations because the shared `__push` helper
   uses it.
5. ~~**`6g` Flip `intentc emit/run/build` to the SSA path**~~ —
   done 2026-05-19. `intentc emit/run/build` (both
   `--backend=c` and `--backend=llvm`) now route through
   new `emit_c_via_ssa` / `emit_llvm_via_ssa` helpers in
   [src/main.rs](src/main.rs); each tries the SSA pipeline
   first and falls back to the tree backend when SSA can't
   represent the program safely. A module-wide
   `ssa_path_supports(&TypedProgram)` gate walks every
   function up front: param/return types must avoid
   Vec/Array/Channel/Atomic/Mutex/Guard/OwnedStr (the SSA
   ABI for ref-wrapped aggregates is not yet aligned with
   tree's by-value convention); statements must avoid
   parallel-for, Tasks, multi-item Print, Str-literal Print
   items, and Assert with a custom message; expressions
   must avoid OwnedStr-producing operations (Str + Str
   concat). If any pattern hits, the helper bypasses SSA
   entirely and emits via the tree backend. SSA-LLVM also
   gained three correctness fixes along the way:
   `intent_print` materializes a per-arg-type format-string
   global (`%lld\\n`/`%g\\n`/`%s\\n`/`%d\\n`) and calls
   `@printf`; `Terminator::Return(Some(op))` now uses the
   function's declared return type (was hardcoded `ret
   i64`); `Const::Float` materialization uses `fadd <T> 0.0,
   c` and `const_str` ensures float literals always carry a
   `.0`/`e` (LLVM rejects integer constants in float
   contexts). All 439 lib + 47 e2e tests green; ~half the
   examples now flow through SSA-C / SSA-LLVM and the rest
   gracefully use the tree backends. Remaining follow-ups
   listed below (each separately landable).
5a. ~~**SSA-LLVM `Ref(Vec)` / `Ref(Array)` param ABI**~~ —
    done 2026-05-19. Vec ABI: `llvm_type_string(Ref(Vec))`
    keeps the `%intent_vec_<T>*` (matches tree-LLVM); the
    body handlers for `Index` / `Len` / `Drop` now call a
    new `vec_aggregate_operand` helper which, when the
    operand's static type is `Ref(Vec)` / `RefMut(Vec)`,
    emits a `load %intent_vec_<T>, %intent_vec_<T>* …`
    before the existing `extractvalue` so the aggregate is
    in hand. Array ABI: `llvm_type_string(Ref(Array))` now
    short-circuits to `[N x T]*` (a single `*` — matching
    tree) rather than `[N x T]**`, because SSA-LLVM's
    Array values already live as `[N x T]*` alloca
    pointers so a "reference to an array" is the same
    LLVM-level pointer; `array_pointer_operand` is the
    pass-through that documents the invariant. Two
    correctness fixes surfaced once the gate dropped its
    Vec/Array clause: (a) `emit_cast` now compares
    `llvm_type(from) == llvm_type(to)` instead of source-
    language type equality, so `i64 → u64` produces an
    identity copy (`add 0`) rather than an invalid `zext
    i64 to i64`; (b) Call argument types now consult a
    per-module `fn_sigs: BTreeMap<String, (Vec<Type>,
    Type)>` (built once at the start of `emit`) so a
    Const operand passed to a Vec-returning function
    resolves to the callee's declared parameter type
    instead of the call's return type. The
    `ssa_path_supports` gate in `main.rs` dropped its
    `Type::Vec(_) | Type::Array { .. }` clauses; the
    `borrows`, `iterate`, `control_flow`, `early_exit`,
    `bounds_elision`, `for_loops`, and `invariants`
    examples now flow through SSA-LLVM end-to-end. All
    439 lib + 47 e2e tests green.
5b. ~~**SSA-side Print with Str literal items + multi-item shape**~~
    — done 2026-05-19. `TypedStmt::Print` in `lower_stmts`
    now emits a sequence that mirrors tree-LLVM's shape:
    one `intent_print_item(<value>)` per item (no trailing
    newline), an `intent_print_putc(32)` between items, and
    a single `intent_print_putc(10)` after all items. Str
    items become a `StrLit` IR instruction whose value is
    passed to `intent_print_item`. Both SSA backends
    recognize the new synthetic call names: SSA-C emits
    `printf("%fmt", cast(val));` (no `\n`) for items and
    `putchar((int)(c));` for putc; SSA-LLVM emits an
    inlined private format-string global + `@printf` for
    items, and `trunc i64 → i32` + `@putchar` for putc.
    The `intent_print` name is retired from SSA lowering.
    The gate dropped its "single-Expr-only" Print clause;
    every example with multi-item Print now flows through
    SSA. Two new gate clauses landed in expr-level checks
    to keep the strings example honest until SSA proper:
    (a) `Binary{Eq|Ne|Lt|Le|Gt|Ge}` whose `left.ty` is
    `Str`/`OwnedStr` (tree uses `strcmp`; SSA-LLVM would
    emit a wrong pointer compare), and (b) `Len { array }`
    whose `array.ty` is `Str`/`OwnedStr` (tree uses
    `strlen`; SSA emits the compile-time length constant
    which is 0 for non-Array). Both fall back to tree.
    All 439 lib + 47 e2e tests green.
5b-followup. **SSA strcmp + strlen lowering for Str** —
    close the two gate clauses introduced in 5b. Requires
    a synthetic `intent_str_eq` / `intent_str_len` call (or
    a direct strcmp/strlen IR instruction) plus matching
    SSA-C / SSA-LLVM emit. ~1 session.
5b-followup. ~~**SSA `strcmp` / `strlen` for Str**~~ — done
    2026-05-19. `lower_expr_to_operand` now detects
    `Binary{Eq|Ne|Lt|Le|Gt|Ge}` with Str/OwnedStr operands
    and emits a synthetic `intent_str_cmp(l, r) -> i64`
    call followed by a Binary comparison against zero;
    `Len { array }` over a Str/OwnedStr operand becomes a
    synthetic `intent_str_len(s) -> u64` call. Backends
    recognize the names: SSA-C emits
    `v_<n> = (int64_t)strcmp(…);` and
    `v_<n> = (uint64_t)strlen(…);`; SSA-LLVM declares
    `@strcmp(i8*, i8*) -> i32` + `@strlen(i8*) -> i64` and
    emits `call i32 @strcmp …; sext i32 → i64` and `call
    i64 @strlen …`. The two sub-gate clauses in `main.rs`
    are gone — `strings.intent` now flows through SSA on
    both backends.
5c. ~~**SSA `OwnedStr` + assert-with-message**~~ — done
    2026-05-19. **Assert-with-message:** SSA's `Assert`
    lowering already attached a `StrLit` arg to the
    synthetic `intent_assert_fail` call; the backends now
    use it. SSA-C emits
    `fprintf(stderr, "assertion failed: %s\n", msg); abort();`
    when an arg is present; SSA-LLVM emits a private
    `@.str.<n>` format-string global + `call i32 (i32,
    i8*, ...) @dprintf(i32 2, ...)` + `@abort` + `unreachable`.
    The `@dprintf` extern was added to the preamble. The
    gate's `message.is_none()` clause is gone.
    **OwnedStr / concat:** SSA's `Binary{Add}` lowering on
    Str/OwnedStr operands now emits a 4-arg
    `intent_str_concat(l, l_owned, r, r_owned)` call —
    each `_owned` flag is a compile-time `Const::Int(0/1)`
    based on whether the operand's type is `OwnedStr`.
    Backends call the shared runtime helper:
    `backend_c::emit_intent_str_concat_c` and
    `backend_llvm::emit_intent_str_concat_definition` were
    promoted to `pub(crate)` and emitted from both tree
    and SSA preambles (the tree backends used to inline
    their own definitions; the inline copies are gone).
    SSA Drop of `OwnedStr` emits `free` (C:
    `free((void*)v);`; LLVM: `call void @free(i8* …)`).
    The gate's `Type::OwnedStr` clause is gone —
    `strings_concat.intent` and `assert_messages.intent`
    now flow through SSA-C and SSA-LLVM end-to-end. All
    439 lib + 47 e2e tests green.
5a. **Windows-native threading + Win32 syscall paths** —
   C-side done 2026-05-19; LLVM-side still pending.
   **C-side (done):** New `intent_thread_t` /
   `intent_thread_create` / `intent_thread_join` /
   `intent_thread_yield` wrappers in the tree-C preamble
   dispatch on `#if defined(_WIN32)`. The Win32 arm uses
   `CreateThread` / `WaitForSingleObject` / `CloseHandle`
   / `SwitchToThread` (`<windows.h>` + `<synchapi.h>`); the
   POSIX arm continues with `pthread_create` /
   `pthread_join` / `sched_yield`. Task spawn/join sites
   in `emit_stmt` now call the wrappers instead of pthread
   directly. The mutex `lock` / `unlock` helpers gained a
   `_WIN32` arm next to the existing `__linux__` arm,
   parking via `WaitOnAddress` / `WakeByAddressSingle` /
   `WakeByAddressAll` against the same Drepper three-state
   protocol; other platforms fall back to the
   `intent_thread_yield` backoff. intentc's `build` driver
   gates `-pthread` on `!cfg!(target_os = "windows")` and
   adds `-lsynchronization` on Windows. Two existing tests
   were updated to match the new spelling
   (`task_spawn_lowers_to_pthread_create_with_outlined_body`
   asserts both arms appear; the mutex test asserts both
   the Linux and Windows wait/wake symbols). All 439 lib +
   47 e2e tests green.
   **LLVM-side (done 2026-05-19):** Tree-LLVM threading
   externs are now `host_uses_win32_threading()`-gated. On
   Windows hosts the preamble declares `@CreateThread` /
   `@WaitForSingleObject` / `@CloseHandle` /
   `@SwitchToThread` / `@WaitOnAddress` /
   `@WakeByAddressSingle`; on POSIX hosts it stays with
   `@pthread_create` / `@pthread_join` / `@sched_yield` /
   `@syscall(...)`. Four call sites in `backend_llvm.rs`
   gained Win32 arms next to their POSIX bodies:
   - **Spawn** (`emit_task_via_pthread`): on Win32, calls
     `@CreateThread(null, 0, fn, ctx, 0, null)` returning
     a HANDLE (i8*), then `ptrtoint`s the handle into the
     existing `i64` handle slot of `%intent_task_handle`.
   - **Join** (`TypedStmt::TaskJoin`): on Win32, loads the
     i64 slot, `inttoptr`s it back to an i8* HANDLE, calls
     `@WaitForSingleObject(h, -1)` (INFINITE), then
     `@CloseHandle(h)`.
   - **Mutex park** (`mutex_lock` body): on Win32, allocas
     an i32 compare slot, stores 2, bitcasts both
     `locked_p` and the compare slot to i8*, calls
     `@WaitOnAddress(addr_i8, cmp_i8, 4, -1)`.
   - **Mutex wake** (`Guard` drop): on Win32, bitcasts the
     locked i32 pointer to i8* and calls
     `@WakeByAddressSingle(locked_i8)` — no state-reset
     load needed (the prior `store atomic 0` already
     released the lock).
   The `%intent_task_handle` struct shape (`{ i64, i8* }`)
   is unchanged across platforms. Two new cfg-gated tests
   (`mutex_lock_uses_wait_on_address_on_windows`,
   `task_spawn_uses_create_thread_on_windows`) pin the
   Windows IR shapes; the existing futex test gained
   `#[cfg(not(target_os = "windows"))]` and is paired with
   the new Windows variant.
   Cross-compilation is still out of scope for v1 (the host
   `target_os` is baked into the emitted .ll); a `--target=`
   flag is the natural follow-up but not blocking.
   **LLVM parallel-for Windows path (done 2026-05-20):** both
   tree-LLVM (`emit_parallel_for_via_gomp`) and SSA-LLVM
   (`emit_parallel_for_region_llvm` +
   `emit_outlined_parallel_for`) now branch on
   `host_uses_win32_threading()`. On Windows the call site
   open-codes a hardcoded N=4 `@CreateThread` fan-out (tid 0
   runs synchronously on the calling thread; tids 1..3 are
   spawned via `@CreateThread(null, 0, fn, &warg, 0, null)`,
   joined with `@WaitForSingleObject(h, -1)`, and released
   with `@CloseHandle(h)`). Each thread receives a
   `WinParArg { i8* ctx, i64 tid, i64 nt }` per-thread struct
   (instead of the shared ctx pointer GOMP passes); the
   outlined fn switches its signature to
   `i8* @__intent_par_<N>(i8* %_arg)` to match CreateThread's
   start-routine ABI and unpacks tid/nt from the WinParArg
   instead of calling `omp_get_thread_num` /
   `omp_get_num_threads`. The preamble omits the
   `@GOMP_parallel`/`omp_get_*` declares on Windows. The Win32
   thread count is hardcoded at N=4 for v1; a future revision
   can plumb a runtime lookup through the existing WinParArg
   without changing the outlined-fn shape. Two new host-gated
   lib tests (`parallel_for_uses_create_thread_fanout_on_windows`
   in both tree-LLVM and SSA-LLVM) pin the new shape.
6. **`8` Cranelift backend** — multi-week, deferred.
7. **`9` Direct-asm targets (x86_64-linux first)** — multi-week,
   deferred.
8. ~~**Verifier #4: constant tracking cleared after `if`/`while`**~~ —
   done 2026-05-20. `clear_constants` (blanket-clear-every-
   binding) replaced with `clear_constants_for(env, names)` at
   the four merge sites (if/else, while, for, for-iter). A new
   `collect_branch_mutations` walks the body recursively (into
   nested if/while/for/for-iter bodies) and collects the names
   that could mutate the outer binding: `Stmt::Assign` LHS,
   `Stmt::IndexAssign` LHS, and `&mut <name>` argument targets
   reachable through any sub-expression. Shadow-`let`s aren't
   collected — the inner shadow dies with the inner scope.
   Effect: bindings the body provably never touched keep their
   `info.constant`, so downstream `prove` discharges via the
   layer-1 constant-fold fast path instead of round-tripping to
   SMT. Two new lib tests
   (`constant_tracking_survives_unrelated_if_else`,
   `constant_tracking_cleared_when_body_reassigns`) pin the
   precision boundary. 441 → 443 lib tests; 47 e2e unchanged.
125. ~~**Nested affine struct fields + recursive Drop**~~ —
     done 2026-05-22. Closes D2 from the drop-chaining queue.
     `struct Outer { inner: Inner, … }` where `Inner` is
     non-Copy (has heap fields) now compiles, with both
     backends recursively walking struct types at scope-
     exit Drop time.
     - **Checker gate** in [src/checker.rs](src/checker.rs)
       admits `Type::Struct(_)` as a struct field type
       (was rejected as "non-Copy" before).
     - **Non-Copy registry fixed-point**: replaced the
       single-pass population of `STRUCT_NON_COPY_REGISTRY`
       with a fixed-point loop so source order doesn't
       determine whether an outer struct gets marked
       affine. Without this, `struct Outer { inner: Inner }`
       declared before `struct Inner { s: OwnedStr }`
       would slip through.
     - **C backend**: extracted `emit_struct_field_drops`
       helper from the existing Drop-arm body. Recurses
       through nested struct fields (`Type::Struct(_)`
       arm GEPs into the field via path concatenation and
       calls itself).
     - **LLVM backend**: parallel
       `emit_llvm_struct_field_drops` helper. The
       `Type::Struct(_)` arm GEPs into the field pointer
       and recursively walks. Uses
       `LLVM_STRUCT_FIELDS_REGISTRY` to look up the inner
       struct's fields.
     - **Nested-path move gate**: `let v = o.inner.s;` for
       a non-Copy field is rejected with a clean
       diagnostic + workaround hint (move the inner
       struct out first). Without this gate the dual-Drop
       (binding + outer-struct path) would double-free.
       Implemented as `is_nested_field_access` check at
       the let-stmt site.
     - **2 lib tests + 1 example added**:
       `nested_affine_struct_field_compiles_with_recursive_drop`
       asserts the C output contains the chained
       `free((void*)v_o.inner.s)`;
       `nested_path_move_of_non_copy_field_rejected` pins
       the gate;
       [examples/nested_struct_drop.intent](examples/nested_struct_drop.intent)
       exercises a `Outer { id, body: Inner { label:
       OwnedStr, counts: Vec<i64> } }` flow.
     - **Still pending**: nested-path move tracking (would
       lift the diagnostic above); deeper RAII patterns.
     798 → 800 lib tests; 47 e2e stable.

124. ~~**`Mutex<T>` + `Channel<T, N>` enum payloads**~~ —
     done 2026-05-22. Symmetric to closure #123's
     struct-field work. Mutex and Channel are inline
     struct layouts with no Drop concern; gate-lift +
     payload-zero literal extension are the only changes
     needed.
     - **Checker gate** in [src/checker.rs](src/checker.rs)
       admits `Type::Mutex(_)` and `Type::Channel(_, _)`
       as enum payloads alongside the previously-supported
       types. Diagnostic narrowed to call out Guard<T> as
       the single remaining gap.
     - **LLVM payload-zero literal** extended:
       `Type::Mutex(_)` and `Type::Channel(_, _)` join the
       `zeroinitializer` list alongside Vec / Tuple /
       Struct / Array / Task.
     - **No Drop emission needed**: Mutex and Channel
       payloads are stack-shaped; the existing enum-Drop
       dispatch correctly emits a no-op for these payload
       kinds. (Only OwnedStr / Vec payloads have heap-
       conditional frees.)
     - **1 lib test added**:
       `enum_mutex_payload_compiles`. The Channel case
       parallels Mutex's shape; not separately tested to
       avoid the channel-literal type-inference gotcha
       (`channel()` needs an explicit type hint or
       turbofish that's tracked elsewhere).
     - **Still pending**: Guard<T> as struct field /
       enum payload (RAII unlock wiring).
     797 → 798 lib tests; 47 e2e stable.

123. ~~**`Mutex<T>` + `Channel<T, N>` struct fields**~~ —
     done 2026-05-22. Combined with closure #102's
     field-borrow (`ref s.m`), users can now build
     mutex-guarded state structures.
     - **Checker gate** in [src/checker.rs](src/checker.rs)
       admits `Type::Mutex(_)` and `Type::Channel(_, _)`
       alongside the previously-supported affine field
       types. Only `Type::Guard(_)` remains rejected — its
       RAII unlock is bespoke and needs more wiring through
       the struct-Drop path.
     - **Backends unchanged**: Mutex and Channel are inline
       struct layouts (`{ value, locked }` / Vyukov MPSC
       ring buffer slots). The existing struct typedef +
       field-Drop emission paths already handle them
       — Mutex / Channel don't carry heap data themselves
       so the per-field Drop is a no-op.
     - **Field-borrow integration**: `mutex_lock(ref s.m)`
       and `channel_send(ref s.ch, value)` flow through
       closure #102's `RefField` machinery cleanly.
     - **2 existing lib tests refreshed**:
       `struct_mutex_field_still_rejected` →
       `struct_mutex_field_compiles_and_locks`; new
       `struct_guard_field_still_rejected` pins the
       remaining gate. Diagnostic text updated to reflect
       the new Guard-only rejection.
     - **Still pending**: `Guard<T>` as struct field
       (needs RAII-unlock wiring through struct Drop);
       Mutex / Guard / Channel as enum payloads (would
       follow the same path).
     796 → 797 lib tests; 47 e2e stable.

122. ~~**Task + Atomic enum payloads**~~ — done 2026-05-22.
     Last two of the originally-listed affine payload
     types now work. Only Mutex / Guard / Channel payloads
     remain rejected. Both new types have no Drop story
     (Task drops via join in v1's sequential lowering;
     Atomic is a primitive cell).
     - **Checker gate** in [src/checker.rs](src/checker.rs)
       admits `Type::Task` and `Type::Atomic(_)` alongside
       the previously-supported types.
     - **LLVM `llvm_type_string` gained a Task arm**:
       returns `"%intent_task_handle"` (the existing
       `{ i64, i8* }` struct from the module preamble).
       Without this, the EnumVariantWithPayload codegen for
       Task hit `llvm_type`'s aggregate-unreachable arm.
     - **LLVM payload-zero literal**: extended the
       `zeroinitializer` arm to include `Type::Task`
       alongside Vec / Tuple / Struct / Array. Atomic
       payload types already fold via `atomic_storage_llvm`
       which returns a scalar integer width — they use
       `0` (the existing `_ => "0"` fallback) cleanly.
     - **2 lib tests added**:
       `enum_atomic_payload_compiles` (Atomic<i64> as
       payload), `enum_task_payload_compiles` (Task as
       payload).
     794 → 796 lib tests; 47 e2e stable.

121. ~~**Const initializer arithmetic**~~ — done 2026-05-22.
     `const B: i64 = A + 1;` (and `*`, `-`, `/`, `%`) now
     folds at parse + check time across previously-declared
     integer consts. Closes another README small-item.
     - **Checker** `literal_const_value` in
       [src/checker.rs](src/checker.rs) gained two arms:
       `Var(name)` looks up a previously-validated const
       (whose type must match the new const's declared
       type); `Binary { op: +/-/*/'/'/%, .. }` recursively
       folds both operands and applies the checked
       arithmetic. Overflow / div-by-zero / type mismatch
       return None, falling through to the existing
       "must be a literal value" diagnostic.
     - **Parser** `expr_as_int_literal` in
       [src/parser.rs](src/parser.rs) mirrors the checker
       fold (same five binary ops + Var lookup) so the
       resolved integer value flows into the
       `[T; SIZE]` array-length resolver (closure #120).
     - **2 lib tests rewritten / added**: the legacy
       `const_decl_rejects_non_literal_initializer` and
       `const_initialized_with_other_const_rejected`
       tests now assert the positive cases
       (`*_compiles`).
     794 lib tests; 47 e2e stable.

120. ~~**`const N` as `[T; N]` array length**~~ — done
     2026-05-22. Closes a long-standing README small-item.
     Users can declare `const SIZE: i64 = 8;` and use it in
     array types: `let xs: [i64; SIZE] = …`.
     - **Parser** in [src/parser.rs](src/parser.rs) gained
       a `const_int_values: HashMap<String, i128>` map.
       Populated by `parse_const_decl` when the initializer
       is an integer literal (including the `-N` form).
       `parse_type` consults the map when the array-length
       slot has an identifier; falls back to a clean
       "must be a literal integer or a previously-declared
       const" error for unknown / forward references /
       non-literal const initializers.
     - **Works across all type-position contexts**: `let`
       annotations, fn params, struct fields, and array
       literals are all parsed via the same `parse_type`
       so they pick up const-N uniformly.
     - **2 lib tests rewritten / added**: the legacy
       `const_cannot_be_array_length` became
       `const_as_array_length_compiles` (positive case);
       a new `unknown_const_in_array_length_rejected`
       pins the diagnostic for undeclared identifiers.
     793 → 794 lib tests; 47 e2e stable.

119. ~~**`[T; N]` enum payload**~~ — done 2026-05-22.
     Closes another follow-up from #113. Arrays of Copy
     elements as enum payloads need no Drop (stack
     lifetime); both backends handle layout via small
     fixes parallel to the struct-field array work.
     - **Checker gate** in [src/checker.rs](src/checker.rs)
       admits `Type::Array { element, .. }` when `element.is_copy()`.
       Task / Atomic still rejected.
     - **C-side typedef** in
       [src/backend_c.rs](src/backend_c.rs) uses
       `format_declarator(payload_ty, "payload")` for array
       payloads — emits `int64_t payload[4]` (inline) instead
       of an `intent_arr<N>_<T>` typedef. Mirrors the
       struct-field array fix from closure #100.
     - **C-side payload literal**: `EnumVariantWithPayload`
       detects an inline `ArrayLit` payload and uses a
       bare-brace `{e1, e2, …}` initializer instead of the
       ArrayLit's `((T[N]){…})` compound-literal form
       (which can't init a struct field of array type).
     - **LLVM payload-zero**: extended the
       `zeroinitializer` arm to include `Type::Array { .. }`
       (was Vec / Tuple / Struct only).
     - **1 lib test added**:
       `enum_array_payload_compiles` asserts the C output
       contains the inline `int64_t payload[4]`
       declarator.
     - **New example**
       [examples/enum_arr_payload.intent](examples/enum_arr_payload.intent)
       exercises a `Window { Open([i64;4]), Closed }` flow.
       Wired into the cross-backend e2e test.
     - **Still pending**: Task / Atomic enum payloads;
       non-Copy destructure-binding extraction.
     792 → 793 lib tests; 47 e2e stable.

118. ~~**Vec<T> enum payload (heap-aware Drop)**~~ — done
     2026-05-22. Extends closure #113 (OwnedStr enum
     payload) to admit `Vec<T>`. Closes one of the four
     follow-ups noted in #113.
     - **Checker gate lifted** in [src/checker.rs](src/checker.rs):
       `Vec<T>` joins `OwnedStr` as a permitted enum
       payload. Other affine types (`[T;N]` / `Task` /
       `Atomic`) still need more work.
     - **`ENUM_NON_COPY_REGISTRY`** population extended:
       enums with a Vec payload are also registered as
       affine.
     - **C backend Drop emission** in
       [src/backend_c.rs](src/backend_c.rs) gained a Vec
       branch in the enum-Drop dispatch — uses
       `intent_vec_<T>__free(.payload)` instead of
       `free((void*).payload)` for Vec payloads.
     - **LLVM backend Drop emission** in
       [src/backend_llvm.rs](src/backend_llvm.rs) parallel:
       extract the Vec struct via `extractvalue`, call
       `@intent_vec_<tag>__free` on it.
     - **C struct-typedef ordering** pre-pass extended:
       enum payload types may also reference Vec; the
       `struct_field_vec_elements` collector now walks
       `program.enums` payload types too so the Vec
       typedef lands before the enum typedef that
       references it. (Same shape as the closure #100 fix
       for struct field Vec.)
     - **LLVM payload-zero literal**: `EnumVariant` for a
       payload-less variant of a Vec-payloaded enum now
       uses `zeroinitializer` instead of `0` (which LLVM
       rejects for aggregate types). Same fix applies to
       Tuple / Struct payload aggregates.
     - **C payload-zero literal**: `EnumVariant` for a
       payload-less variant uses `{0}` (empty designated
       initializer) for aggregate payloads instead of bare
       `0` (C can't init a struct from an integer).
     - **1 lib test added**:
       `enum_vec_payload_compiles_and_drops` asserts the
       Vec free helper appears in C output.
     - **New example**
       [examples/enum_vec_payload.intent](examples/enum_vec_payload.intent)
       exercises a `Bag { Items(Vec<i64>), Empty }`-style
       flow. Wired into the cross-backend e2e test.
     - **Still pending**: [T;N] / Task / Atomic enum
       payloads; destructure-binding extraction for non-
       Copy payloads (chained move tracking).
     791 → 792 lib tests; 47 e2e stable.

117. ~~**SSA bool-print parity**~~ — done 2026-05-22. Bool
     prints through both SSA backends now render as
     "true"/"false" (was: "1"/"0"). Closes the last
     SSA-vs-tree print divergence noted in the README
     small-items list.
     - **SSA-C** (`src/ssa_backend_c.rs`): the `Type::Bool`
       branch of `intent_print_item` now emits
       `fputs((v) ? "true" : "false", stdout)` instead of
       `printf("%d", (int)v)`. Mirrors tree-C.
     - **SSA-LLVM** (`src/ssa_backend_llvm.rs`): synthesizes
       two private string globals `@.bool_true` /
       `@.bool_false` (idempotent within a module), then
       uses `select i1 %v, i8* @.bool_true, i8* @.bool_false`
       to pick the right pointer and `printf("%s", …)` to
       print. Mirrors tree-LLVM.
     - **1 lib test added**:
       `ssa_bool_print_renders_true_false` asserts the C
       output contains the `"true"` literal.
     790 → 791 lib tests; 47 e2e stable.

116. ~~**Empty struct + bare-block scope-stmt**~~ — done
     2026-05-22. Two small README items in one closure.
     - **Empty struct (`struct E {}`)**: checker no longer
       rejects `decl.fields.is_empty()` — cap is now
       `0..=64`. Parser's struct-lit lookahead extended to
       accept `Type { }` (RBrace immediately after LBrace)
       in addition to the existing `Type { field: …}`
       shape. Useful for marker types (zero-sized
       tag-only structs).
     - **Bare `{ … }` as scope-stmt**: parser used to emit
       "bare blocks aren't supported in v1" with a
       workaround hint. Now desugars at parse time to
       `Stmt::If { cond: Bool(true), then_body: <stmts>,
       else_body: [] }`. The existing If-scope machinery
       handles binding visibility, affine moves, and
       codegen. Constant-fold collapses the `if true`
       branch in both backends.
     - **2 lib tests rewritten**:
       `empty_struct_rejected` →
       `empty_struct_compiles`,
       `bare_block_statement_surfaces_helpful_diagnostic` →
       `bare_block_statement_compiles`. The `1..=64`
       diagnostic text updated to `0..=64`.
     788 → 790 lib tests; 47 e2e stable.

115. ~~**Unit-return functions: `fn f() { … }`**~~ — done
     2026-05-22. Procedures (side-effect-only functions)
     no longer need `-> i64` + `return 0;` boilerplate.
     Closes a README small-item.
     - **Parser** in [src/parser.rs](src/parser.rs):
       `parse_function` detects the missing `->` arrow
       after the closing `)` and falls back to
       `return_type = Type::I64`. After parsing the body,
       it appends a synthetic `Stmt::Return { Int(0) }`
       if the last stmt isn't already a Return — idempotent.
     - **No IR / checker / backend changes**: the
       resulting Function carries `return_type: I64` and a
       body ending with `return 0;`, indistinguishable
       from a user-written `fn f() -> i64 { …; return 0;
       }`. The user-facing surface is the sugar.
     - **2 lib tests added**:
       `unit_return_function_compiles_and_runs` (positive
       basic case) and
       `unit_return_function_already_has_return` (early-
       exit + idempotent synthesis).
     - **New example**
       [examples/unit_return.intent](examples/unit_return.intent)
       demonstrates a few procedure shapes (`greet()`,
       `announce(label, n)`, `run_demo()`). Wired into
       the cross-backend e2e test.
     788 → 790 lib tests; 47 e2e stable.

114. ~~**Type-associated functions `Type.helper(args)`**~~ —
     done 2026-05-22. Closes the README small-item "no
     syntax for `Type.helper()`". Constructors and other
     type-namespaced helpers now work without falling back
     to free functions.
     - **Checker** in [src/checker.rs](src/checker.rs):
       removed the `must take 'self' as its first parameter`
       gate in `hoist_methods_into_functions`. Self-less
       methods are hoisted to the same `<TypeName>_<method>`
       mangled name as their self-bearing siblings.
     - **MethodCall handler** gained a new arm: when the
       receiver is a `Var` naming a declared struct OR
       enum AND `<TypeName>_<method>` is in the signature
       table, the call rewrites to a direct
       `Call { name: "<T>_<method>", args }` (no
       self-receiver prefix). Args are type-checked against
       the function's declared parameter types in order;
       arity mismatches surface a clean diagnostic.
     - **Co-exists with `recv.method()`**: a single
       `methods on Point { fn new(x,y) -> Point …; fn sum(self: Point) -> i64 … }`
       block can declare both shapes. `Point.new(1, 2)`
       dispatches via the type-associated path; `p.sum()`
       dispatches via the existing receiver path.
     - **1 lib test added**:
       `type_associated_function_compiles_and_dispatches`
       asserts the program compiles and the C output
       contains a direct `fn_B_make(…)` call. The legacy
       `method_without_self_clean_diagnostic` was rewritten
       — the rejected case no longer exists.
     - **New example**
       [examples/type_associated_fn.intent](examples/type_associated_fn.intent)
       shows a `Point.new(x, y)` constructor + a
       `Point.origin()` zero-arg helper + a regular
       `p.sum()` method. Wired into the cross-backend
       e2e test.
     788 lib tests; 47 e2e stable.

113. ~~**Enum payloads admit OwnedStr (heap-aware Drop)**~~ —
     done 2026-05-22. Closes a soundness gap: previously the
     enum-payload validation rejected any non-Copy type with
     "v1 enum payloads are Copy-only", citing the unfinished
     T1.2 struct RAII work — which has since landed (closures
     #98 / #100). This closure lifts OwnedStr through the
     gate and wires the heap-conditional Drop in both
     backends.
     - **Checker gate lifted** in
       [src/checker.rs](src/checker.rs): `OwnedStr` now
       passes the per-variant payload validation. Other
       affine payload types (Vec / [T;N] / Task / Atomic)
       remain rejected.
     - **`ENUM_NON_COPY_REGISTRY`** added to
       [src/ast.rs](src/ast.rs) (parallel to the existing
       `STRUCT_NON_COPY_REGISTRY`). `Type::Enum(name)` now
       consults it for `is_copy()`, so enums with a heap
       payload are correctly affine and the scope-exit Drop
       pass fires on them.
     - **C backend Drop emission** in
       [src/backend_c.rs](src/backend_c.rs) gains a
       `Type::Enum(name)` arm that emits a `switch (v.tag) {
       case <i>: free((void*)v.payload); break; default:
       break; }` for the variant tags that carry a payload.
       Tag list comes from a new `ENUM_PAYLOAD_TAGS_REGISTRY`
       thread-local populated at emit start.
     - **LLVM backend Drop emission** in
       [src/backend_llvm.rs](src/backend_llvm.rs) gains the
       same arm: extract tag + payload via `extractvalue`,
       OR-fold per-tag `icmp eq` comparisons into a single
       i1, branch to a free-then-done block. Mirror
       `LLVM_ENUM_PAYLOAD_TAGS_REGISTRY` thread-local.
     - **LLVM payload-zero fix**: `EnumVariant` for a
       payload-less variant of a payloaded enum was emitting
       `insertvalue ... i8* 0, 1` for OwnedStr (i8*) — LLVM
       requires `null` not `0` for pointer literals. Fixed.
     - **v1 limitation**: destructure-binding patterns
       (`Some(s)`) for non-Copy payloads are rejected with
       a clean diagnostic. Match the variant tag without a
       binding (`Maybe.Some` not `Maybe.Some(s)`) for
       OwnedStr payloads. The alias-vs-Drop interaction
       (binding `s` shares the heap pointer with the enum's
       payload) would cause double-free / UAF without
       chained move tracking, which is deferred.
     - **2 lib tests added**:
       `enum_owned_str_payload_compiles_and_drops` (positive
       case + asserts the tag-conditional `free` in C
       output) and `enum_non_copy_payload_binding_rejected`
       (the destructure restriction).
     - **New example**
       [examples/enum_owned_payload.intent](examples/enum_owned_payload.intent)
       exercises a `Maybe<OwnedStr>`-style pattern flowing
       through `make()` → consume in `classify()` → scope-
       exit Drop. Wired into the cross-backend e2e test.
     - **Still pending**: Vec / [T;N] / Task / Atomic
       payload types; destructure-binding extraction for
       non-Copy payloads (needs chained move tracking).
     786 → 788 lib tests; 47 e2e stable.

112. ~~**Deep field paths for `xs[i].a.b = v`**~~ — done
     2026-05-22. Closure #109 shipped single-level paths;
     this lifts the depth restriction. The existing
     checker loop already iterates each segment with
     per-step type descent + Copy check; only the
     `field_path.len() > 1` gate needed removal. Backends
     already loop over segments (both C's
     `.field1.field2…` suffix and LLVM's chained GEP).
     - **Checker** in [src/checker.rs](src/checker.rs):
       removed the explicit `if field_path.len() > 1` gate.
       Per-segment Copy check still enforces the invariant
       (intermediate segments must be Copy structs; leaf
       must be Copy).
     - **Backends**: no changes. The existing emission
       loops handle arbitrary path length.
     - **Example refreshed**:
       [examples/mixed_place_assign.intent](examples/mixed_place_assign.intent)
       now includes a `Outer { inner: Inner }` two-level
       update alongside the original single-level cases.
     - **Lib test rewritten**: the legacy
       `index_then_field_assign_no_field_unsupported`
       test became
       `index_then_field_assign_deep_path_compiles`
       (positive case).
     786 lib tests; 47 e2e stable.

111. ~~**Match on `Str` / `OwnedStr` scrutinee**~~ — done
     2026-05-22. Pattern::Str was scaffolded in closure #110
     and gated with a "not yet supported" diagnostic; this
     closure lowers it.
     - **`check_match_str` helper** in
       [src/checker.rs](src/checker.rs): detects `Str` /
       `OwnedStr` scrutinee at the start of the Match
       check and desugars to:
       ```
       {
         let __match_str_<span> = <scrutinee>;
         if __match_str == "a" { body_a }
         else if __match_str == "b" { body_b }
         else { wildcard_body }
       }
       ```
       The scrutinee binds to a temp once so any
       side-effecting expression evaluates exactly once.
     - **Exhaustiveness**: wildcard arm required (the
       string space is open). Missing wildcard surfaces
       "non-exhaustive match: string scrutinees require a
       wildcard `_ then …` arm".
     - **Duplicate / unreachable-arm checks** carry the
       same shape as the integer/bool path.
     - **No backend changes** — desugar uses existing
       `==` on Str/OwnedStr (strcmp-based) + IfExpr + Block
       primitives. Both tree backends and the SSA fallback
       work as-is.
     - **2 lib tests added**:
       `match_str_dispatch_with_wildcard` (positive case
       through both backends) and
       `match_str_missing_wildcard_rejected` (the
       diagnostic).
     - **New example**
       [examples/match_str.intent](examples/match_str.intent)
       exercises Str and OwnedStr scrutinees with `==`
       returning a content-based comparison. Wired into
       the cross-backend e2e test.
     784 → 786 lib tests; 47 e2e stable.

110. ~~**Match on `bool` scrutinee**~~ — done 2026-05-22.
     `match b { true then …, false then …, _ then … }` now
     works. Exhaustiveness requires both arms OR a wildcard.
     The dispatch reuses the existing integer-switch shape
     (true=1, false=0) so no backend changes were needed.
     - **`Pattern::Bool(bool)`** added to [src/ast.rs](src/ast.rs)
       alongside the existing Int/Variant/Wildcard variants.
       Also added `Pattern::Str(String)` for the upcoming
       string-match work; currently the checker emits a
       "not yet supported" diagnostic for Str patterns.
     - **Parser** recognizes `true` / `false` / `"…"` tokens
       as patterns in match arms.
     - **Checker** in [src/checker.rs](src/checker.rs):
       new dispatch kind `is_bool_dispatch` accepts a `bool`
       scrutinee; `Pattern::Bool` patterns are encoded into
       `int_value=0/1` so the backends switch uniformly.
       Exhaustiveness checks both `true` and `false` arms
       are present (or a wildcard). Duplicate-arm detection
       and unreachable-after-wildcard diagnostics carry
       over verbatim.
     - **No backend changes** — the switch lowering already
       handles any integer-typed scrutinee, and
       `llvm_type_string(Type::Bool)` is `i1` which is a
       legal switch scrutinee type. C `switch (bool)` is
       fine too.
     - **Format pass** (`format.rs`) prints `true`/`false`
       and `"text"` patterns.
     - **SMT encoder** in `src/smt.rs` rejects Bool/Str
       patterns with a clean "not yet supported" message.
       Bool exhaustiveness via `if/else` substitution can
       be added later if there's a real proof need.
     - **2 lib tests added**:
       `match_bool_compiles_and_dispatches` (positive case)
       and `match_bool_nonexhaustive_rejected` (missing-arm
       diagnostic). The old `match_bool_pattern_rejected`
       test was rewritten — the rejected case no longer
       exists.
     - **New example**
       [examples/match_bool.intent](examples/match_bool.intent)
       exercises both explicit `true`/`false` arms and
       `true`/`_` (wildcard) arms. Wired into the
       cross-backend e2e test.
     - **Str patterns** (`"foo" then …`) still surface a
       gated "not yet supported" diagnostic; the strcmp-
       dispatch lowering is the natural follow-up closure.
     783 → 784 lib tests; 47 e2e stable.

109. ~~**`xs[i].field = v` mixed-place assignment**~~ — done
     2026-05-22. The parser was already gated with a workaround
     diagnostic; this closure replaces the gate with real
     lowering through both backends. Single-level paths only
     in v1; the Copy-leaf restriction keeps the codegen
     simple (no field-drop on overwrite).
     - **AST + IR**: `Stmt::IndexAssign` and
       `TypedStmt::IndexAssign` gained `field_path` fields
       (`Vec<String>` AST-side; `Vec<(String, u32)>` IR-side
       with resolved indices). Empty path falls back to plain
       `xs[i] = v;`.
     - **Parser**: the `looks_like_index_then_field_assign`
       lookahead now routes to the new
       `parse_index_then_field_assign_stmt` instead of
       erroring. It parses one-or-more `.<field>` segments
       before the `=`. The OLD diagnostic is gone.
     - **Checker**: validates each segment against the
       element type's struct declaration. Errors if the
       element is non-struct, the field is unknown, the
       leaf field is non-Copy (would need field-Drop on
       overwrite — deferred), or the path is deeper than
       one level. The leaf field's type drives the value
       coercion (instead of the element type).
     - **C backend** in
       [src/backend_c.rs](src/backend_c.rs):
       `emit_index_assign` gained a `field_path` parameter
       and builds `.field1.field2…` suffix to append after
       the indexed access. Works uniformly for owned
       `[T;N]`, `Vec<T>`, and `&mut` variants.
     - **LLVM backend** in
       [src/backend_llvm.rs](src/backend_llvm.rs): both
       the Vec and Array arms now walk the resolved
       `field_path`, GEP'ing through each `i64 0, i32
       <field_index>` segment and storing at the leaf
       pointer. The struct type for each GEP comes from
       `LLVM_STRUCT_FIELDS_REGISTRY`. Also switched
       `llvm_type(element)` to `llvm_type_string` for the
       Vec data pointer so aggregate elements like
       `Struct("Point")` don't panic.
     - **SSA gate** in `src/ssa.rs`: rejects non-empty
       `field_path` with a `LowerError` so programs route
       through the tree backend.
     - **2 existing lib tests updated**:
       `index_then_field_assign_gated_with_workaround` →
       `index_then_field_assign_compiles_and_mutates` (now
       asserts the program compiles); a new
       `index_then_field_assign_no_field_unsupported` test
       pins the deep-path-still-rejected diagnostic.
     - **New example**
       [examples/mixed_place_assign.intent](examples/mixed_place_assign.intent)
       exercises both `Vec<Point>` and `[Point; 3]` cases;
       wired into the cross-backend e2e test.
     782 → 783 lib tests; 47 e2e stable.

108. ~~**In-place `push(mut ref xs, v)`**~~ — done 2026-05-22.
     Adds a second form of `push` that operates through a
     pointer to the Vec instead of consuming + returning it.
     Closes the last partial-move polish gap noted in
     closure #105.
     - **`check_push_builtin`** in [src/checker.rs](src/checker.rs):
       dispatches on the first arg's type. `Vec<T>` → existing
       consuming form (returns Vec<T>); `mut ref Vec<T>` →
       new in-place form (returns `i64` = new length, doesn't
       consume the source). Diagnostic updated to mention
       both forms.
     - **C runtime** in [src/backend_c.rs](src/backend_c.rs)
       gained a per-element `intent_vec_<T>__push_mut(Vec*,
       T)` helper alongside the existing `__push`. Same realloc
       logic, but mutates through the pointer instead of
       returning a new struct value.
     - **LLVM runtime** in
       [src/backend_llvm.rs](src/backend_llvm.rs) gained
       `@intent_vec_<tag>__push_mut(%intent_vec_<tag>*, T)
       -> i64` with GEP'd loads + stores into the Vec
       struct's data/len/cap slots. Reuses the existing
       grow-or-store basic-block structure.
     - **Call-site dispatch** in both backends: the
       `push_mut` Call name routes to the in-place helper.
       LLVM's `vec_element_of_first_arg` already derefs
       through refs, so the existing tag-resolution path
       works.
     - **SSA gate** in [src/main.rs](src/main.rs): rejects
       `Call("push_mut", ...)` so programs using the
       in-place form route through the tree backend
       (SSA-C / SSA-LLVM don't lower push_mut yet).
     - **2 lib tests added**:
       `push_mut_through_struct_field` (asserts the
       `__push_mut` helper appears in C output for the
       through-field call) and `push_mut_local_binding`
       (asserts the in-place form works on a local Vec).
     - **New example**
       [examples/push_mut.intent](examples/push_mut.intent)
       exercises both shapes (local + struct field). Wired
       into the cross-backend e2e test.
     780 → 782 lib tests; 47 e2e stable.

107. ~~**Tuple auto-equality (compiler-derived `==`)**~~ — done
     2026-05-22. Closes the last of the
     struct/enum/tuple `==` triad — tuples are anonymous so
     they can't have a user `Eq` impl, but the checker can
     synthesize an AND-chain of per-element comparisons.
     - **`check_equality`** in [src/checker.rs](src/checker.rs):
       new branch matches `(Type::Tuple(l), Type::Tuple(r))`
       with equal arity AND equal element types. Walks the
       element types and synthesizes a chain of
       `TupleAccess(lhs, i) == TupleAccess(rhs, i)`
       comparisons, AND-joined into a single typed-IR
       expression. The chain returns the final `&&` result
       (or a single comparison for arity 1, or `true` for
       arity 0).
     - **Recursive dispatch** for nominal element types:
       when an element is a `Struct` or `Enum`, the
       per-element comparison emits `Call { "<T>_eq", [a,
       b] }` directly (must have a matching `Eq` impl in
       scope; missing impl surfaces a clean per-element
       diagnostic). Primitive element types use the
       built-in `TypedExprKind::Binary { Eq }` form.
     - **Tuple-diagnostic refreshed**: the "tuples have
       no built-in `==`" path now only fires when shapes
       don't match (different arity or element types).
       The default case is success.
     - **2 lib tests added**:
       `tuple_equality_field_by_field_desugar` (the
       (i64, i64) case) and
       `tuple_equality_of_struct_routes_through_eq_impl`
       (asserts the C output contains 2 `fn_Point_eq`
       calls for `(Point, Point)` equality). The old
       `tuple_equality_rejected_with_targeted_diagnostic`
       was replaced.
     - **New example**
       [examples/tuple_eq.intent](examples/tuple_eq.intent)
       exercises three shapes: `(i64, i64)`,
       `(bool, i64, i64)`, and `(Point, Point)`. Wired
       into the cross-backend e2e test.
     779 → 780 lib tests; 47 e2e stable.

106. ~~**Enum `==` desugar + partial-then-whole-move diagnostic**~~ —
     done 2026-05-22. Two follow-ups in one closure.
     - **Enum `==` via user Eq**: `check_equality` now matches
       `(Type::Enum(l), Type::Enum(r))` in addition to the
       Struct/Struct case. Same dispatch shape: `<E>_eq(a, b)`
       for `==`, `!<E>_eq(a, b)` for `!=`. The diagnostic for
       enums-without-impl updated to point at the
       `implement Eq for E` recipe.
     - **`resolve_enum_types_in_program` walks impls**: the
       enum-name → `Type::Enum` resolver previously walked
       methods_blocks but not `program.impls`. The Eq impl
       body's `(self as i32)` cast was rejected because `self`
       still typed as `Type::Struct("Color")` (the parser's
       default for any nominal name). New arm walks impls'
       for_type + method signatures + method bodies.
     - **Partial-then-whole-move diagnostic**:
       `diagnose_partial_then_whole_move` helper emits a
       "cannot move 'b' — its field 'f' was previously moved
       out" diagnostic at every Var-consume site where the
       binding has a non-empty `moved_fields` map. Wired in
       front of each `consume_if_moved_var` callsite (8
       sites) via a mechanical regex pass.
     - **2 lib tests added**:
       `enum_eq_via_user_impl` (end-to-end Color equality
       through hoisted `Color_eq`) and
       `partial_move_whole_after_field_rejected` (the new
       diagnostic).
     - **New example**
       [examples/enum_eq.intent](examples/enum_eq.intent)
       exercises the pattern with `(self as i32)` for
       payload-less enums. Wired into the cross-backend e2e
       test.
     777 → 779 lib tests; 47 e2e stable.

105. ~~**Partial-move tracking for struct fields**~~ — done
     2026-05-21. `let taken = bag.contents;` moves the Vec
     field out of the struct without invalidating the rest
     of the struct. Closes the bulk of the T1.2 phase 2b
     polish queue.
     - **`VarInfo.moved_fields: BTreeMap<String, Span>`** in
       [src/checker.rs](src/checker.rs) tracks which fields
       have been moved out of a struct binding. Populated
       at every consume site (let RHS, function arg,
       struct-lit field init, return value).
     - **`consume_if_moved_var` extended** with a
       `FieldAccess { object: Var, field, ... }` arm that
       inserts the field into the base binding's
       `moved_fields` instead of marking the whole binding
       moved. Symmetric with the existing Var arm.
     - **FieldAccess read** consults `moved_fields` and
       surfaces a use-after-move diagnostic with a "moved
       here" related-span when the field is already in the
       map.
     - **`TypedStmt::Drop` extended** with a `moved_fields:
       Vec<String>` field. Populated at both scope-exit
       Drop emission sites (the inline pass and the
       Return-path cleanup) from the binding's
       `moved_fields` snapshot.
     - **Per-backend Drop emit** in both
       [src/backend_c.rs](src/backend_c.rs) and
       [src/backend_llvm.rs](src/backend_llvm.rs) consults
       the Drop's `moved_fields` list and skips any matching
       field in the per-field free pass. Avoids the
       double-free that would otherwise result from
       `taken` (the new binding owning the Vec) and the
       struct's own scope-exit walk both freeing the same
       buffer.
     - **2 lib tests added**:
       `partial_move_field_extract_compiles` (asserts the
       struct's moved field is NOT freed; only the new
       binding is) and `partial_move_double_extract_rejected`
       (use-after-move diagnostic).
     - **New example**
       [examples/partial_move.intent](examples/partial_move.intent)
       exercises the pattern end-to-end; wired into the
       cross-backend e2e test.
     - **Still pending**: moving the struct as a whole
       after a partial move (diagnose at the Var-consume
       site); `mut ref t.xs` + `push(...)` combined
       semantics; nested field moves (`t.inner.f`).
     775 → 777 lib tests; 47 e2e stable.

104. ~~**Auto `==` for structs via `implement Eq for T`**~~ —
     done 2026-05-21. `a == b` and `a != b` on two bindings
     of the same struct type desugar to a call to the
     hoisted `<T>_eq` function. Convention is `fn eq(self: T,
     other: T) -> bool`. Tuple / enum auto-equality follow
     the same recipe and can be added when the use cases
     surface.
     - **Checker desugar** in [src/checker.rs](src/checker.rs):
       `check_equality` now takes `signatures` and, when
       both operands are the same struct type, looks up
       `<T>_eq` in the signature table. If found with
       signature `(T, T) -> bool`, the operator is rewritten:
       `Eq` → `Call { name: "<T>_eq", args: [lhs, rhs] }`;
       `Ne` → `Unary { Not, Call { … } }`. Falls through to
       the existing diagnostic when no impl exists.
     - **Diagnostic refreshed**: the struct-equality
       message now points at `implement Eq for T { fn eq…
       }` as the canonical recipe (was: "wait for user-
       defined equality").
     - **1 lib test added**: `struct_eq_via_user_impl` pins
       end-to-end `a == b` / `a != b` resolving through
       `Point_eq`. Existing
       `struct_equality_rejected_with_targeted_diagnostic`
       still asserts the no-impl path.
     - **New example**
       [examples/struct_eq.intent](examples/struct_eq.intent)
       exercises the pattern; wired into the cross-backend
       e2e test.
     774 → 775 lib tests; 47 e2e stable.

103. ~~**Reverse-declaration field drop order**~~ — done
     2026-05-21. Struct Drop now walks fields in reverse
     declaration order, mirroring construction (Rust's
     RAII convention). Pure code-shape change in both
     backends' Drop emit (`fields.into_iter().rev()` /
     `.iter().enumerate().rev()`). One new lib test
     `struct_drop_reverse_field_order` asserts a Pair
     with two OwnedStr fields emits `free(v_p.second)`
     before `free(v_p.first)`.
     773 → 774 lib tests; 47 e2e stable.

102. ~~**Field-borrow expressions — `ref t.f` / `mut ref t.f`**~~ —
     done 2026-05-21. Single-level borrow of a struct field
     so atomic operations work through a struct that owns
     the cell. T1.2 phase 2b follow-up.
     - **Parser unchanged** — it was already producing
       `ExprKind::Ref { inner: FieldAccess(Var, "f") }` for
       `ref t.f`. Only the checker rejected it ("can only
       borrow a named variable").
     - **Checker** in [src/checker.rs](src/checker.rs):
       `check_ref` and `check_ref_mut` gained a branch that
       inspects `inner.kind` for `FieldAccess { object:
       Var(name), field, ... }`. Validates the base is a
       struct binding, the field exists, the base isn't
       moved (and for mut ref, not behind a `ref T`).
       Returns the new typed variants.
     - **IR**: two new `TypedExprKind` variants in
       [src/ir.rs](src/ir.rs) — `RefField { object, field,
       field_index }` and `RefMutField { same shape }`.
       Wrapper type carries `Type::Ref(field_ty)` /
       `Type::RefMut(field_ty)`. Single-level only in v1
       (no `ref a.b.c`); deeper paths land later.
     - **Tree-C backend** in
       [src/backend_c.rs](src/backend_c.rs): emit `&v_t.f`
       for primitive/aggregate field types or just
       `v_t.f` for array fields (C array-decay applies).
     - **Tree-LLVM backend** in
       [src/backend_llvm.rs](src/backend_llvm.rs): GEP into
       the struct's alloca with `i64 0, i32 <field_index>`
       and return the resulting field pointer as the SSA
       operand. The `walk_expr` capture helper picks up
       the base binding (so an outlined parallel-for body
       can carry the struct address through its ctx
       struct).
     - **SSA path** in [src/ssa.rs](src/ssa.rs): surfaces
       `LowerError` to route programs through the tree
       backend (parallel to other struct-related
       lowerings). The SSA `expr_kind_name` debug helper
       gained the matching arms.
     - **Other walkers updated**: `typed_to_expr` in
       [src/checker.rs](src/checker.rs) (round-trips
       `RefField` back to `Expr::Ref { FieldAccess(...) }`);
       `expr_ssa_supported` in [src/main.rs](src/main.rs)
       (routes these through the tree backend); the LLVM
       string-interning pre-pass's no-op arm.
     - **2 lib tests added**:
       `field_borrow_unlocks_atomic_through_struct` (the
       end-to-end Atomic-through-struct example);
       `field_borrow_rejects_non_struct_base` (the
       diagnostic when the base isn't a struct).
     - **New example**
       [examples/struct_atomic_field.intent](examples/struct_atomic_field.intent)
       exercises `atomic_store(mut ref c.hits, …)`,
       `atomic_load(ref c.hits)`, and `atomic_fetch_add`
       through the field-borrow. Wired into the cross-
       backend e2e test.
     - **Vec field push still blocked**: `push(mut ref
       b.xs, 30)` fails because `push` expects `Vec<T>`
       by value (consuming). Unblocking it needs either
       partial-move tracking on struct fields (let
       expressions split `let xs = t.xs; ... t.xs = xs;`)
       or a new mutating-ref vec API. Tracked as a #3
       polish follow-up.
     771 → 773 lib tests; 47 e2e stable.

101. ~~**#8 Phase 2: user-Drop auto-call at scope exit**~~ —
     done 2026-05-21. Wires `implement Drop for T` into the
     existing scope-exit drop machinery so the user's
     `T_drop(self)` runs automatically. Closes the last
     pending follow-up of multi-session item #8.
     - **Affine-aggregate registry** in [src/checker.rs](src/checker.rs)
       extended: any struct/enum with an `_drop` hoisted impl
       is registered as non-Copy alongside structs that carry
       non-Copy fields. Without this, a Copy-only struct
       (`struct Resource { id: i64, open: bool }`) would report
       `is_copy() = true` and the scope-exit pass would skip
       Drop emission entirely. The registry consults the
       hoisted function table (`<T>_drop`) rather than
       `program.impls`, which is drained by
       `hoist_impls_into_functions` before this pre-pass runs.
     - **`no_drop` flag on `self` inside `<T>_drop`**: a hoisted
       Drop impl's `self: T` parameter must NOT trigger another
       auto-Drop at scope exit (would infinite-recurse).
       `check_function` now sets `VarInfo.no_drop = true` for
       the `self` param when the enclosing function name ends
       with `_drop` and the param is a struct/enum type. The
       existing `no_drop` semantics (designed for iteration-
       view aliasing) cleanly extends here — body reads still
       resolve normally; the scope-exit pass skips the auto-
       Drop.
     - **Two drop-emit sites consult `no_drop`**: the existing
       scope-exit pass (~line 3082) and the Return-path
       cleanup (~line 3498). The latter was previously
       filtering only on `is_copy + moved` and would re-emit a
       Drop on `self` immediately before the user's `return
       self.id;` — re-introducing the recursion. Both sites
       now check `info.no_drop` first.
     - **Per-backend user-Drop registries**: new thread-local
       `USER_DROP_REGISTRY` in [src/backend_c.rs](src/backend_c.rs)
       and `LLVM_USER_DROP_REGISTRY` in
       [src/backend_llvm.rs](src/backend_llvm.rs) populated at
       emit start by scanning `program.functions` for names
       ending `_drop`. Each backend's `TypedStmt::Drop {
       ty: Type::Struct(name) }` arm now consults the registry
       and conditionally emits `(void)fn_<T>_drop(v_<binding>);`
       (C) or the equivalent `call i64 @fn_<T>_drop(%Struct_<T>
       %loaded)` (LLVM) instead of the per-field-free pass.
     - **Per-field free skipped when user Drop auto-called**:
       to avoid double-free, the auto-call runs ONLY when the
       struct has no heap-shaped fields (no OwnedStr, no
       Vec<T>). Structs with both a user Drop AND heap fields
       still get the per-field free pass; users must invoke
       `t.drop()` manually if they want richer behavior. A
       future closure can lift this by changing the Drop
       signature to `fn drop(mut self: T)` so the user can
       observe/mutate fields before the per-field free runs.
     - **1 lib test added**:
       `user_drop_auto_call_at_scope_exit` pins the auto-call
       emission for a Copy-only struct (no heap fields).
       Existing test
       `drop_interface_validates_signature` still pins the
       signature-validation half of the Drop interface.
     - **Example updated**:
       [examples/drop_interface.intent](examples/drop_interface.intent)
       no longer threads through a manual `r.drop()` call; the
       binding goes out of scope at the end of `use_resource`
       and the runtime prints "releasing resource" thanks to
       the auto-call. Wired into the cross-backend e2e test.
     - **Remaining**: user Drop for structs with heap fields
       (design call needed — `mut self` vs shadow-copy split);
       Drop on enum payloads; Drop chaining when struct
       contains another Drop-implementing struct.
     770 → 771 lib tests; 47 e2e stable.

100. ~~**#3 expanded: affine struct fields — Vec / [T;N] / Task /
     Atomic**~~ — done 2026-05-21. Builds on closure #98's
     OwnedStr-field MVP to admit the remaining affine field
     types short of Mutex / Guard / Channel.
     - **Checker gate lifted** in [src/checker.rs](src/checker.rs)
       to accept `[T; N]` of Copy elements, `Vec<T>`, `Task`,
       and `Atomic<T>` as struct fields. The diagnostic for
       still-rejected types now reads "Mutex / Guard / Channel
       need explicit wiring" instead of the broader earlier
       list.
     - **STRUCT_NON_COPY_REGISTRY** population generalized from
       the OwnedStr-only check to "any non-Copy field" so the
       aggregate correctly reports affine for any of the new
       supported field types.
     - **C backend Drop emission** in
       [src/backend_c.rs](src/backend_c.rs) gains a `Type::Vec`
       arm that routes through the existing
       `intent_vec_<T>__free` helper against `v_<binding>.<field>`.
       Array / Atomic / Task fields stay no-op at Drop time —
       they're stack-shaped and the surrounding affine binding's
       move tracking is the only resource discipline they need.
     - **C struct typedef ordering fix**: a struct with a Vec
       field needs `intent_vec_<T>__from`'s typedef in scope by
       the time the struct typedef is parsed. A new pre-struct
       pass walks `program.structs` for Vec element types and
       emits those bundles before the struct typedefs; the
       existing post-struct Vec emission tracks an
       `emitted_vec_bundles` set so it doesn't re-emit the
       same bundle for the function-body pass. `Vec<Struct>`
       ordering is preserved (the post-struct emission still
       handles those).
     - **C struct field array layout**: struct fields of
       `[T; N]` type now use `format_declarator` (inline `T
       name[N]` declarator) instead of the `intent_arr<N>_<T>`
       typedef. C forbids assigning a compound-literal array
       value to a struct member of array type; the StructLit
       emit detects an inline `ArrayLit` field initializer and
       emits a bare-brace `{e1, e2, …}` initializer
       (designated-init compatible) instead of going through
       ArrayLit's `((T[N]){…})` compound-literal form.
     - **LLVM Drop emission** in
       [src/backend_llvm.rs](src/backend_llvm.rs) gains the
       Vec arm parallel to the C side: GEP the field, load the
       Vec struct value, call `@intent_vec_<T>__free`. Stack-
       shaped fields no-op.
     - **LLVM FieldAccess-as-Index-base**: the existing Index
       lowering only handled `Var` bases. Added a new
       FieldAccess-base arm that reuses `emit_lvalue_addr` to
       get a pointer to the field's aggregate, then GEPs into
       it with `i64 0, i64 <idx>` and loads the element. Fixes
       the long-standing `t.data[i]` panic that blocked the
       previous attempt (closure #85, reverted).
     - **String / Vec collectors recurse into all sub-expressions**:
       `collect_vec_elements_in_expr` in both backends gained
       the same StructLit / Tuple / FieldAccess / Match /
       IfExpr / Block / EnumVariantWithPayload arms that the
       LLVM string-interning fix (closure #98) added. Without
       these, a Vec constructed inside a nested expression
       position (e.g. struct field initialized via match arm)
       wouldn't have its `intent_vec_<T>` helper bundle
       generated.
     - **3 lib tests added**: `struct_array_field_compiles_and_indexes`
       (inline `T name[N]` declarator + FieldAccess-base
       indexing), `struct_vec_field_compiles_and_drops` (Vec
       typedef hoisted, per-field free in C output),
       `struct_mutex_field_still_rejected` (Mutex / Guard /
       Channel still rejected with the new diagnostic
       wording — replaces the obsolete
       `struct_non_copy_field_rejected` test).
     - **New example**
       [examples/struct_mixed_fields.intent](examples/struct_mixed_fields.intent)
       exercises a Frame with mixed Copy + [i64;8] + OwnedStr
       fields plus a Bag with a Vec field. Wired into the
       cross-backend e2e test. 768 → 770 lib tests; 47 e2e
       stable.
     - **Remaining for #3 polish**: field-borrow expressions
       (`ref t.xs`, `mut ref c.hits`), partial-move tracking,
       reverse-declaration drop order, Mutex / Guard / Channel
       struct fields.

99. ~~**#7 Phase 2 (half): bounded generics —
    `fn min<T>(…) where T is Cmp`**~~ — done 2026-05-21.
    First half of the #7 Phase 2 closure. Vtables + first-
    class interface objects deferred to a follow-up.
    - **WIP gate removed** from
      `monomorphize_generics_in_program` in
      [src/checker.rs](src/checker.rs). Templates with
      where-clauses now flow through the specializer
      instead of surfacing a "T1.5 phase 2 still in
      progress" diagnostic at parse-clean programs.
    - **Bound-existence check** at monomorphization time:
      for each `(template, concrete_type)` pair needing
      specialization, the checker walks
      `program.impls` and verifies a matching
      `implement <Iface> for <concrete_type>` decl exists.
      Missing impls surface a targeted diagnostic
      ("generic function 'min' requires `T is Cmp`, but
      no `implement Cmp for Score` is in scope. Add the
      impl or pick a type that satisfies the bound.")
      pointing at the where-clause span.
    - **Scope-aware first-arg inference**: the previous
      monomorphizer pre-pass only inferred T from
      literal first args (Int / Float / Bool). It now
      threads a per-fn local-binding map (function
      params + annotated `let` bindings) through both
      the collector and the rewriter, so a call like
      `let m: Score = min(a, b);` where `a: Score` is a
      bound variable correctly infers `T = Score` and
      both pre-passes agree on the specialization name.
    - **`type_mangle` extended** to handle
      `Type::Struct(name)` / `Type::Enum(name)` (return
      `Struct_<Name>` / `Enum_<Name>`) so the
      specialization symbol is a valid LLVM/C
      identifier instead of falling through to the
      `Debug`-with-replace fallback that emitted
      illegal characters.
    - **Non-generic-fn where-clause gate** clarified:
      keeps a diagnostic for `fn f(...) where T is X`
      where `f` has no `<T>`, but with a different
      message ("where-bounds apply only to generic type
      parameters") since the old WIP wording no longer
      fits.
    - **2 lib tests added**: `where_bound_satisfied_compiles`
      (end-to-end compile of `min<T> where T is Cmp` against
      `implement Cmp for Score`); `where_bound_unsatisfied_rejected`
      (missing impl produces the right diagnostic).
      Two existing tests (`where_bound_parses_but_gated`,
      `where_clause_trailing_comma_accepted`) were updated to
      assert the new ship state.
    - **New example**
      [examples/bounded_generics.intent](examples/bounded_generics.intent)
      exercises `min<T>` and `max<T>` against
      `implement Cmp for Score`, run end-to-end through
      both backends. Wired into the cross-backend
      e2e test. 767 → 768 lib tests; 47 e2e stable.
    - **Remaining for #7 Phase 2**: vtables / dynamic
      dispatch (first-class interface objects), auto-`==`
      via `Eq` impls for struct/tuple/enum.

98. ~~**#3 MVP: OwnedStr struct fields with auto-drop +
    LLVM string-interning bug fix**~~ — done 2026-05-21.
    Closes multi-session item #3 at the MVP level. The
    last standalone item in the queue.
    - **The blocker**: the previous attempt (closure
      #85, reverted) hit a strlen segfault when an
      OwnedStr-field struct ran through default LLVM.
      Diagnosis this session: the emitted IR contained
      `call i8* @intent_str_concat(i8* null, i32 0, i8*
      null, i32 0)` — both string literal operands had
      lowered to `null`. Root cause was
      `collect_strings_in_expr` in
      [src/backend_llvm.rs](src/backend_llvm.rs) — the
      module-level pre-pass that hoists every string
      literal into a `@.print_str.<n>` private global
      — only recursed into `Unary`/`Binary`/`Call`/
      `Cast`/`Index`/`Len`/`ArrayLit` and silently
      dropped everything else through `_ => {}`. Inside
      a `StructLit` field initializer (or `Tuple`,
      `Match`, `IfExpr`, `Block`, …) the literals never
      got registered, so the Str-arm of `emit_expr`
      fell back to the stub `"null"` placeholder.
    - **String-interning fix**: explicit arms for every
      sub-expression form that can contain string
      literals — `StructLit { fields }`,
      `Tuple { elements }`, `TupleAccess { tuple }`,
      `FieldAccess { object }`,
      `EnumVariantWithPayload { payload }`,
      `Match { scrutinee, arms }`,
      `IfExpr { cond, then_value, else_value }`,
      `Block { stmts, tail }`,
      `CallIndirect { callee, args }`. Leaves only
      Int / Float / Bool / Var / Ref / RefMut / FnRef
      / EnumVariant (none of which carry payloads) in
      the no-op case. Fix is leverage for every
      future feature that nests string literals
      anywhere — not just structs.
    - **Affine-aggregate registry**: new thread-local
      `STRUCT_NON_COPY_REGISTRY` in [src/ast.rs](src/ast.rs)
      (parallel to the existing enum payload
      registries) tracks struct names whose
      declaration carries at least one OwnedStr field.
      The checker populates it with a pre-pass before
      `Type::is_copy` is consulted on individual
      fields; `Type::is_copy(Type::Struct(name))` now
      consults it and reports `false` for affine
      aggregates. Without this, the struct would still
      be Copy and no Drop IR would be emitted at
      scope exit.
    - **Per-backend struct-fields registry**: parallel
      `STRUCT_FIELDS_REGISTRY` in
      [src/backend_c.rs](src/backend_c.rs) and
      `LLVM_STRUCT_FIELDS_REGISTRY` in
      [src/backend_llvm.rs](src/backend_llvm.rs) (both
      thread-local, populated from `program.structs` at
      emit start). The `TypedStmt::Drop` handler in
      each backend now has a `Type::Struct(name)` arm
      that walks the registered field list and emits a
      free for each owning field: C uses
      `free((void*)v_<binding>.<field>)`; LLVM uses
      `getelementptr %Struct_<Name>, ... i32 <idx>` +
      `load i8*` + `call void @free(i8*)`.
    - **Move tracking through struct literals**: the
      checker's `StructLit` arm now calls
      `consume_if_moved_var` on each field initializer
      (parallel to the existing `Call`/`Let` move
      sites). Without this, a heap string passed
      `caller → fn-param → struct-field` would be
      freed twice — once when the fn param goes out of
      scope and once when the returned struct's field
      is dropped at the caller's scope. With this, the
      affine binding flows cleanly through the chain.
    - **The narrow MVP gate**: still only `OwnedStr`
      passes the struct-field test. Other affine
      types (`Vec<T>`, `[T;N]`, `Task`, `Atomic<T>`,
      `Channel`, `Mutex`, `Guard`) stay rejected with
      a clearer diagnostic: "struct field `T::f` has
      non-Copy type X — v1 supports Copy types and
      OwnedStr as struct fields; other affine types
      (Vec / [T;N] / Task / Atomic) need more codegen
      work". The phase-1-gate lib test was updated
      accordingly.
    - **1 new lib test**:
      `struct_owned_str_field_compiles_and_drops`
      pins type-checking + per-field free emission
      in C output for a 3-field (i64, OwnedStr, bool)
      struct with one binding moved through a
      function call and one bound to a literal
      concat.
    - **New example**
      [examples/struct_owned_field.intent](examples/struct_owned_field.intent)
      exercises both shapes end-to-end. Runs cleanly
      through both backends:
      ```
      tag id= 7  name= release-v1  active= true
      tag id= 42  name= alpha-beta  active= false
      ```
      No double-free, no segfault. 766 → 767 lib tests;
      47 e2e + 3 integration unchanged. **Remaining for
      #3 (deferred)**: non-OwnedStr affine fields
      (Vec / Array / Task / Atomic), reverse-declaration
      drop ordering for multi-field structs, partial-
      move tracking (`drop t.name; …` while keeping
      `t.id`).

97. ~~**Session-end polish: README test totals + feature
    sections refreshed**~~ — done 2026-05-21. Final
    closure of a long session; syncs the README's
    "Supported today" with everything that landed
    closures #81 → #96.
    - **Test totals** updated 731 → 766 lib in the README
      preamble.
    - **Enums** entry updated to reflect payloaded variant
      support landing (was "payloaded gated", now
      "tagged-union codegen lays them out + match
      destructure binds payload").
    - **New Control-flow bullets**: block expressions
      (closure #81) and `try EXPR` (closures #91/#93).
    - **New Generics & interfaces subsection**: generic
      monomorphization (#94), interface dispatch (#95),
      Drop interface phase 1 (#96).
    - All example files cross-linked.
    - **Session summary**: 16 closures landed (#81-#96),
      one revert (#85). Test arc 686 → 766 lib (+80),
      47 e2e stable throughout. Multi-session items
      closed: #1 ✅ Block expressions, #2 ✅ SMT
      modeling, #4 ✅ tagged-union codegen (both
      backends), #5 ✅ try keyword, #6 ✅ generic
      monomorphization, #7 ✅ interface dispatch
      (static), #8 ✅ Phase 1 Drop recognition,
      #9 ✅ Devanagari MVP. **Remaining**: #3 RAII
      non-Copy struct fields (last standalone — needs
      a dedicated session for FieldAccess-as-Index-base
      in tree-LLVM, typedef ordering in tree-C,
      per-field Drop generation, partial-move tracking).
    - **Honest pause point**: I attempted #3 twice
      earlier this session and reverted both times.
      The remaining work needs sustained multi-hour
      focus rather than continuation-style increments.
    766 lib + 47 e2e tests passing.

96. ~~**#8 Phase 1: Drop interface recognition + signature
    validation**~~ — done 2026-05-21. Honest minimal cut
    of multi-session item #8. The auto-call at scope exit
    needs the RAII work for non-Copy structs (#3) to land
    first; until then, users declare `implement Drop for
    T` and call `t.drop()` manually. This phase ensures
    the contract is forward-compatible.
    - **Special-case recognition**: when
      `hoist_impls_into_functions` sees `interface_name
      == "Drop"`, validates the impl:
      - Exactly one method, named `drop`
      - Signature must be `fn drop(self: T) -> i64`
      - Anything else surfaces a targeted diagnostic
    - **The impl hoists normally** to `T_drop` so
      `recv.drop()` dispatches statically (closure #95
      machinery).
    - **3 new lib tests**: valid Drop impl compiles +
      runs; wrong return type rejected; wrong method
      name rejected.
    - **New example**
      [examples/drop_interface.intent](examples/drop_interface.intent)
      shows manual `r.drop()` call from a function body.
      Runs through default LLVM:
      ```
      fn 1 starts
        using resource 7
        releasing resource 7
      fn 1 ends, dropped = 7
      …
      ```
    - **What's deferred (Phase 2)**: auto-call at scope
      exit. Requires #3 to land first — without
      non-Copy structs, the auto-drop pass has no
      meaningful place to fire (structs are currently
      Copy and don't trigger drop). When #3 ships,
      Phase 2 wires the registry of `implement Drop`
      types into the existing `is_copy()` gate and
      drop-stmt emission.
    763 → 766 lib tests; 47 e2e stays green.

95. ~~**#7 Phase 1: interface dispatch (static)**~~ — done
    2026-05-21. Multi-session item #7 closed for static
    dispatch. `interface Iface { fn m(...) -> R; }` +
    `implement Iface for Type { fn m(...) ... }` now
    work end-to-end. Method calls `recv.m()` resolve at
    compile-time based on the receiver's type.
    - **New pre-pass `hoist_impls_into_functions`** runs
      right after `monomorphize_generics_in_program`.
      Validates each impl method's signature against the
      interface declaration, then mangles to
      `<TypeName>_<method>` (same convention as
      `methods on T`). The existing method-dispatch path
      then resolves `recv.m()` to the mangled call
      automatically.
    - **Signature validation**: parameter count + return
      type must match. Mismatches surface targeted
      diagnostics ("impl method 'X::m' has N parameters
      but interface declares M").
    - **Coverage check**: the impl must cover EVERY
      interface method. Missing methods surface a
      diagnostic listing the missing names.
    - **Extra-method rejection**: impl methods not in the
      interface surface "interface 'X' has no method 'Y'".
    - **Collision check**: if `implement Iface for T { fn
      method }` and `methods on T { fn method }` both
      declare the same method, surface a collision
      diagnostic.
    - **Unknown interface**: `implement Mystery for T`
      where `Mystery` isn't declared surfaces a clean
      diagnostic.
    - **for_type validation**: must be a struct or enum
      (nominal type).
    - **2 prior gate tests updated** (`interface_decl_*`,
      `implement_for_*`) to assert successful compilation.
    - **New example**
      [examples/interfaces.intent](examples/interfaces.intent)
      shows `Area for Point` (single-method) and `Bounds
      for Rect` (multi-method) with method dispatch.
      Runs through default LLVM:
      ```
      Point area = 20
      Rect area = 21  perimeter = 20
      ```
    - **What's deferred** (Phase 2): dynamic dispatch /
      vtables; bounded generics over interfaces (`fn
      print<T: Show>(x: T) { x.show(); }` — currently
      `where T is Iface` still gates because the
      monomorphizer doesn't yet specialize the body for
      the impl's method calls).
    763 → 763 lib tests (no net change — 2 gate tests
    rewritten as positive); 47 e2e stays green.

94. ~~**#6 Phase 1: generic monomorphization for pass-through
    generics**~~ — done 2026-05-21. Multi-session item #6
    closed for the most common case. `fn id<T>(x: T) -> T`
    now specializes per call-site literal type and compiles.
    - **New pre-pass `monomorphize_generics_in_program`**
      runs after `desugar_try_let_in_program`. Walks each
      non-generic function for calls to generic templates,
      infers T from the first argument's literal type, and
      builds a unique-by-(fn, type) set of specializations
      to generate.
    - **For each specialization**: clones the template,
      renames to `<fn_name>__<type_mangle>` (e.g.
      `id__i64`, `first__bool`), clears `type_params`,
      and substitutes `Type::Param(T)` → concrete in
      params, return type, and body type annotations
      via `substitute_type_param` and
      `substitute_type_param_in_stmt`.
    - **Call sites** are rewritten via
      `rewrite_generic_calls_in_*` to use the
      specialized name.
    - **Originals dropped**: after specialization,
      `program.functions.retain(|f| f.type_params.is_empty())`
      removes the templates so downstream type-check
      sees a fully-concrete program.
    - **Dead-generic diagnostic**: if a generic template
      has no call sites that inferred its T, surface
      "generic function '%s' is declared but never called
       with concrete types — monomorphization couldn't
       specialize it" so users notice unused generics.
    - **Bounded generics gate**: templates with `where T
      is Iface` clauses skip specialization and surface
      the existing T1.5 phase 2 diagnostic.
    - **V1 restrictions**:
      - Single type parameter only.
      - First call argument must be a literal (Int/Float/
        Bool). Variable arguments need type-check
        context that the pre-pass doesn't have.
      - Body must be type-correct without knowing T
        (pass-through patterns — `fn id<T>`, `fn first<T>`).
        Arithmetic / field access on T needs interface
        bounds (T1.5 phase 2).
    - **5 prior Phase-1 gate tests updated** to assert
      successful specialization instead of WIP gates:
      `generic_id_function_compiles_and_runs_monomorphized`,
      `generic_id_function_specializes_per_concrete_type`,
      `generic_function_unused_surfaces_dead_code_diagnostic`,
      `generic_type_param_trailing_comma_accepted` (now
      asserts dead-generic, not WIP),
      `generic_call_site_specializes_and_compiles`.
    - **New example**
      [examples/generic_functions.intent](examples/generic_functions.intent)
      shows `id<T>` called at three concrete types
      (i64, bool, f64) plus `first<T>`. Runs through
      default LLVM:
      ```
      id(42) = 42
      id(true) = 1
      id(3.5) = 3.5
      first(7, 9) = 7
      ```
    761 → 763 lib tests; 47 e2e stays green.

93. ~~**#5 Phase 2: `try` desugars end-to-end for the
    restricted shape**~~ — done 2026-05-21. Multi-session
    item #5 closed for the common shape; the `try`
    keyword now sugars down to a match-with-early-return
    automatically. Programs like the previous closure's
    `option_error_propagation.intent` can now be rewritten
    one-line-shorter using `try`.
    - **New pre-pass `desugar_try_let_in_program`** in
      [src/checker.rs](src/checker.rs) runs right after
      `hoist_methods_into_functions`. For each function,
      checks if the body matches the restricted shape:
      - `body[0]` is `Stmt::Let { expr: Try { inner }, … }`
      - `body[1..len-1]` are all `Stmt::Let` (block-expr
        in v1 accepts let only)
      - `body[last]` is `Stmt::Return`
    - **When the shape matches**, rewrites the function
      body to a single `Stmt::Return` whose expression is
      a `Match { scrutinee: try_inner, arms: [Some(_t)
      then { let v: T = _t; ...intermediate lets...;
      return-expr }, None then EnumType.None ] }`. The
      Some-arm body is a block expression (closure #81),
      the None-arm body is the enum's payload-less
      variant.
    - **Restrictions enforced**:
      - Function return type must be a known enum.
      - The enum must have exactly one payloaded variant
        and one payload-less variant.
      - Intermediate stmts must all be `let` (anything
        else triggers a diagnostic pointing at the
        `try`-let).
      - `try` outside the restricted shape (e.g. in a
        nested if-body) falls through to the Phase 1
        gate diagnostic.
    - **2 new lib tests**: `try_keyword_desugars_let_try_return_pattern`
      validates the desugar fires;
      `try_keyword_in_unsupported_shape_surfaces_phase_1_gate`
      validates the fallthrough.
    - **New example**
      [examples/try_keyword.intent](examples/try_keyword.intent)
      shows `doubled` and `pipeline` (multi-let chain).
      Runs through default LLVM:
      ```
      doubled(Some(5)) = 10
      doubled(None) defaulted to 99
      pipeline(Some(5)) = 110
      pipeline(None) defaulted to 99
      ```
    760 → 761 lib tests; 47 e2e stays green.

92. ~~**Option error-propagation idiom — example + precedence
    regression**~~ — done 2026-05-21. Small follow-up on
    #91. Until `try` Phase 2 ships the auto-desugar, users
    write the match-with-return pattern manually; this
    closure documents it and locks the parser precedence.
    - **New example file**
      [examples/option_error_propagation.intent](examples/option_error_propagation.intent)
      shows `doubled`, `add_delta`, `double_and_add` over
      `Opt = Some(i64) | None`. Each function uses the
      manual `match { Some(v) then Some(<derived>), None
      then None }` pattern that `try` will sugar to one
      line in Phase 2. Runs through the e2e example
      suite via default LLVM; prints expected output
      and asserts pass.
    - **New lib test** `try_keyword_binds_tightly_to_operand`
      pins parser precedence: `try x + 1` parses as
      `(try x) + 1`, not `try (x + 1)`. Important for
      when Phase 2 lands — wrong precedence would mean
      `try x + 1` tries the SUM instead of extracting
      then adding. Inspects the AST directly (skips
      the checker gate). Locks the parse for future
      refactors.
    - **Edge cases probed** (informally):
      - `try x` where x is i64 (non-enum) → gate fires.
        Phase 2 will type-check the inner and surface a
        targeted "must be a payloaded enum" diagnostic.
      - `try x;` as a bare statement → "expected
        statement" — Try is an expression, the
        closure-#75 discardable-call gate only allows
        Call/MethodCall.
    759 → 760 lib tests; 47 e2e stays green (new
    example picked up by the directory checker).

91. ~~**#5 Phase 1: `try` keyword reserved + parse + walks**~~
    — done 2026-05-21. The full desugar (statement-level
    match-with-early-return) needs surrounding-stmt
    context that `check_expr` doesn't have; deferred to
    Phase 2 as a dedicated session. The keyword is now
    locked in, the AST/IR plumbing exists, and a clean
    WIP gate explains the limitation.
    - **Lexer**: `TokenKind::Try` added; `"try"` ASCII
      keyword reserved.
    - **AST**: new `ExprKind::Try { inner }`.
    - **Parser**: `TokenKind::Try` arm at
      `parse_primary_expr`. Inner parsed at primary-
      expr precedence so `try x + 1` doesn't capture
      the `+ 1`.
    - **Checker `check_expr`** arm: type-checks the
      inner (so type errors there still surface), then
      emits "`try EXPR` is reserved as a keyword but
      the desugar to match-with-early-return is still
      in progress (T2.6 Phase 2). Write the pattern
      manually: `match opt { Opt.Some(v) then v,
      Opt.None then return Opt.None };`"
    - **All recursive walks updated**: substitute_expr,
      expr_mentions, pin_var_to_version, pretty_expr,
      walk_branch_mutations_in_expr, format,
      smt::encode_expr — each recurses into `inner` or
      bails appropriately.
    - **What ships in Phase 2** (next sustained
      session): a stmt-level desugar pass that
      recognizes `let v: T = try opt;` and rewrites
      the surrounding stmt sequence into a match with
      one arm extracting + binding and the other
      `return`-ing the propagated None / Err. Requires
      the enclosing function's return type to match
      the enum type.
    - **1 new lib test** pins the gate.
    758 → 759 lib tests; 47 e2e tests stay green.

90. ~~**#4 Phase 4 LLVM: tagged-union codegen ships in
    default LLVM backend**~~ — done 2026-05-21. Multi-
    session item #4 fully closed. Payloaded enums now
    run end-to-end through `cargo run -- run` (default
    LLVM) and `--backend=c`.
    - **`llvm_type_string(Type::Enum)`** now consults
      `LLVM_ENUM_PAYLOAD_REGISTRY` (thread-local
      mirror of the tree-C one). Payloaded enums route
      to `%Enum_<Name>`; plain enums keep their bare
      `i32` representation.
    - **LLVM preamble** emits `%Enum_<Name> = type {
      i32, <payload> }` per payloaded enum, right
      after struct typedefs.
    - **`EnumVariant` LLVM emit**: payloaded enum's
      payload-less variant builds the struct via two
      `insertvalue`s (tag + zero-init payload). Plain
      enum's variant keeps the literal-tag form.
    - **`EnumVariantWithPayload` LLVM emit**: two
      `insertvalue`s — tag at field 0, the lowered
      payload expression at field 1.
    - **LLVM Match codegen**: detects payloaded
      scrutinees, `extractvalue` field 0 for the
      switch dispatch. For `VariantWithBinding` arms,
      `extractvalue` field 1, `alloca` + `store` the
      result, then register the binding in `ctx.locals`
      with that addr so the arm body's variable reads
      lower to a normal `load` from the alloca. Restore
      the previous `ctx.locals` entry after each arm.
    - **Driver gate removed**: `emit_llvm_via_ssa` now
      detects payloaded-enum programs and forces the
      tree-LLVM path (SSA-LLVM doesn't support enums
      with payloads yet). LLVM path runs cleanly.
    - **Example file** moved from `demo/` back into
      `examples/option_types.intent` — runs through
      the e2e directory-checker test as part of the
      standard example suite.
    - **New lib test** `payloaded_enum_with_match_destructure_compiles_in_llvm`
      validates the default-LLVM path; the prior
      `compile_to_c` test stays to guard the tree-C
      path.
    - 757 → 758 lib tests; 47 e2e tests stay green.

89. ~~**#4 Phase 3 tree-C: tagged-union codegen +
    pattern destructure** (Option<i64>-style enums
    end-to-end)~~ — done 2026-05-21. Payloaded enums
    now compile and run via `--backend=c`. LLVM ships
    in a follow-up.
    - **Checker gate**: lifted for single-Copy-payload
      enums where all payload-bearing variants share
      the same payload type. Multi-field payloads,
      non-Copy payloads, and mixed-type payloads keep
      their existing diagnostics.
    - **`TypedEnumDecl`** now carries
      `payload_types: Vec<Option<Type>>` parallel to
      `variants`. Built once from AST enum decls.
    - **`TypedMatchArm`** now carries
      `binding: Option<(String, Type)>` for
      `VariantWithBinding` patterns. The checker
      pushes a fresh scope, inserts the binding into
      env with the payload type, and pops after
      checking the arm body so the body's reference
      to the binding resolves.
    - **Tree-C preamble**: emits `typedef struct {
      int32_t tag; T payload; } Enum_<Name>;` for each
      payloaded enum (where T is the shared payload
      type). Thread-local
      `ENUM_PAYLOAD_REGISTRY` populated at the start
      of `emit_c` so `c_type_name(Type::Enum)` and
      `format_declarator(Type::Enum)` route payloaded
      enums to the struct typedef.
    - **Tree-C constructors**: `Opt.Some(42)` lowers
      to `(Enum_Opt){.tag = 0, .payload = (42)}`;
      `Opt.None` to `(Enum_Opt){.tag = 1, .payload =
      0}`. Plain enums (no payload variants) keep
      the existing bare `int32_t` tag representation.
    - **Tree-C match codegen**: dispatches on
      `__scr.tag` (after materializing the scrutinee
      into a local). For VariantWithBinding arms,
      the body emits `{ <payload_ty> v_<binding> =
      __scr.payload; __r = (<body>); } break;` so the
      binding is a real C local with payload value.
    - **LLVM driver gate**: `emit_llvm_via_ssa`
      detects payloaded enums in `ir.enums` and
      exits with a clean error pointing at
      `--backend=c`. Avoids hitting the
      `unreachable!()` arms in tree-LLVM until LLVM
      tagged-union codegen ships.
    - **Lib tests updated**: 3 prior Phase 1 gate-tests
      (`enum_variant_with_payload_parses_but_gated`,
      `enum_with_payload_clean_diagnostic`,
      `variant_with_binding_pattern_parses_and_gates`)
      rewritten to assert compilation succeeds (via
      `compile_to_c`) for the supported cases. The
      gate tests for multi-field / non-Copy /
      mixed-type payloads stay.
    - **Demo program**: [demo/option_types.intent](demo/option_types.intent)
      shows `unwrap_or` + `is_some` over `Opt =
      Some(i64) | None`. Lives outside `examples/`
      because the example-runner uses the default
      LLVM backend; the demo runs cleanly via
      `cargo run -- run demo/option_types.intent
      --backend=c` → prints "Some(42) unwrapped =
      42", "None unwrapped with default 100 = 100",
      asserts pass.
    - **What ships in Phase 4** (next session, multi-
      hour): LLVM tagged-union codegen. Same shape as
      tree-C but using `{i32, T}` LLVM struct types,
      `insertvalue` / `extractvalue` for constructor
      and field reads, `switch` on the tag field
      extracted from the scrutinee, alloca for the
      binding's local in each arm.
    - **No net test count change** — 3 tests rewritten
      from gate-assertions to positive assertions, 1
      new test wires the destructure form. 757 lib +
      47 e2e remain green.

88. ~~**#4 Phase 2 (scaffolding only): IR + checker
    constructor + EnumInfo payload types**~~ —
    done 2026-05-21. The "internal" infrastructure for
    tagged-union codegen is in place; the user-facing
    decl gate stays on until backend wire-up ships in
    the next session.
    - **IR**: new `TypedExprKind::EnumVariantWithPayload
      { enum_name, variant, tag, payload, payload_ty }`
      next to the existing `EnumVariant`.
    - **EnumInfo extended**: now carries
      `payload_types: Vec<Option<Type>>` parallel to
      the variant name list. Built once from the AST
      enum decls at checker entry. New
      `lookup_enum_variant_payload(env, enum, variant)`
      helper resolves payload types from the env.
    - **Checker MethodCall arm intercept**:
      `Opt.Some(42)` (parsed as MethodCall by the
      existing parser) is detected when the receiver
      is a Var naming a declared enum and the
      "method" is a variant. Builds
      `TypedExprKind::EnumVariantWithPayload` with the
      payload arg type-coerced to the declared
      payload_ty. Calling a payload-less variant with
      args surfaces a clean diagnostic; missing payload
      arg surfaces a clean diagnostic too.
    - **Walks** (effects walker, typed_to_expr,
      backend walk_expr for capture analysis): all
      have new arms that recurse into `payload`.
    - **Backend arms**: tree-C and tree-LLVM
      `emit_expr` both contain `unreachable!()` arms
      for `EnumVariantWithPayload` — these are
      guarded by the still-active decl gate, so the
      backends never see one in practice. When the
      gate lifts in Phase 3, replace each
      `unreachable!()` with the tagged-union codegen.
    - **SSA**: bails alongside the existing enum bail.
      `main.rs`'s `expr_ssa_supported` marks it as
      unsupported.
    - **Gate**: payloaded enum decls still emit
      "tagged-union codegen + pattern binding are still
       in progress (T1.3 phase 2b backend wire-up)".
       Updated wording to clarify what's left.
    - **No test count change** (the gate keeps user
      programs out of new code paths). 757 lib + 47
      e2e remain green.
    - **What ships in Phase 3** (next session, multi-
      hour): lift the decl gate; emit tagged-union
      struct typedefs (`typedef struct { i32 tag; T
      payload; } Enum_X;` in C, `{i32, T}` in LLVM);
      wire `EnumVariantWithPayload` codegen in both
      backends; match-arm scope introduces the
      binding name with the payload's type;
      extract payload via `.payload` field / LLVM
      `extractvalue`. End-to-end Option<i64>-style
      programs then run.

87. ~~**#4 Phase 1: pattern bindings parse + gate**~~ —
    done 2026-05-21. First slice of multi-session item
    #4 (T1.3 phase 2b tagged-union codegen). The AST /
    parser / checker surface now accepts
    `EnumName.Variant(binding) then …` destructures;
    backend codegen for the actual payload extraction
    ships in Phase 2 of the same multi-session item.
    - **AST**: new `Pattern::VariantWithBinding {
      enum_name, variant, binding }`.
    - **Parser**: extended the match-arm pattern parse
      to consume an optional `(ident)` after the
      variant name. Single-binding form only in v1;
      multi-binding tuple-style (`Pair(x, y)`) tracked
      separately.
    - **Checker**: new arm mirrors the
      `Pattern::Variant` flow for tag lookup +
      seen-variant tracking, then emits a clean WIP
      gate diagnostic: "match arm 'Opt.Some(v …)'
      destructures a payloaded variant — pattern
      bindings parse but tagged-union codegen is still
      in progress (T1.3 phase 2b)".
    - **Format**: pretty-prints `Variant(binding)`
      form correctly; round-trip preserved.
    - **SMT**: rejects with "payloaded variant
      destructure patterns not yet supported in SMT"
      (parallel to the existing enum-variant SMT bail).
    - **2 new lib tests** pin parse + gate +
      format-round-trip.
    - **What ships in Phase 2** (deferred, multi-hour):
      tagged-union layout (`{ i64 tag; union { ... } }`
      in C, `{i32, [N x i8]}` in LLVM); enum
      constructor codegen (`Some(5)` builds the tagged
      struct); match destructure codegen (switch on
      tag + extract payload + bind to `v` in arm
      scope); IR variant
      `TypedExprKind::EnumVariantWithPayload`.
    755 → 757 lib tests; 47 e2e tests stay green.

86. ~~**Devanagari numeral literals — `०१२३४५६७८९`**~~ —
    done 2026-05-21. Follow-up on #83/#85. Completes the
    "code like you speak" surface for Devanagari users —
    numbers can now be written in Devanagari script too.
    - **Lexer dispatch update** in [src/lexer.rs](src/lexer.rs):
      the non-ASCII byte arm now distinguishes Devanagari
      numerals (UTF-8 bytes `0xE0 0xA5 0xA6..=0xAF`) from
      letters and routes them to a new
      `lex_devanagari_number` method instead of
      `lex_unicode_ident`.
    - **`lex_devanagari_number`**: consumes consecutive
      Devanagari digit codepoints, translates each to
      ASCII by subtracting U+0966, parses via
      `i128::from_str_radix`, emits as `TokenKind::Int`.
    - **V1 limits**: integer literals only — no float /
      radix / suffix / underscore-separator support for
      Devanagari digits. ASCII numeric literals retain
      all those features.
    - **Tests**: 3 new lib tests pin single-digit zero,
      multi-digit composition, and arithmetic with mixed
      Devanagari numeral + keyword forms.
    752 → 755 lib tests; 47 e2e tests stay green.
    Example that works:
    ```intent
    फलन main() -> i64 {
      मान x: i64 = ५;       // 5
      मान y: i64 = ४२;      // 42
      खात्री x + y == ४७;
      परत x + y;
    }
    ```

85. ~~**Multi-word Devanagari aliases via post-lex merger**~~
    — done 2026-05-21. Follow-up on closure #83. Five
    multi-word Devanagari phrases now lex as single
    tokens.
    - **New post-lex pass**
      `merge_multi_word_devanagari_aliases(tokens,
      source)` in [src/lexer.rs](src/lexer.rs): walks the
      token list, reads each adjacent token pair's source
      slice via spans (so it sees `तो → Then` after
      single-word resolution as just text), and checks
      whether the combined `"word1 word2"` string matches
      a multi-word alias. If so, replaces the two tokens
      with a single keyword token spanning both. Requires
      the gap between tokens to be whitespace-only.
    - **Aliases supported**:
      `नहीं तो` → Else (Hindi),
      `के लिए` → For (Hindi),
      `सिद्ध करो` → Prove (Hindi),
      `सिद्ध करा` → Prove (Marathi),
      `समान्तर प्रति` → Parallel (Sanskrit).
    - **Conflict handling**: the multi-word form takes
      precedence when both words are present. E.g. `सिद्ध`
      alone is Prove; `सिद्ध करो` is also Prove (no
      conflict in this case). `तो` alone is Then;
      `नहीं तो` is Else (the merger overrides the
      single-word resolution on the first word).
    - **Tests**: 3 new lib tests pin Hindi else / for /
      prove multi-word forms end-to-end.
    749 → 752 lib tests; 47 e2e tests stay green.

84. ~~**SMT method-call discharge — completes #82**~~ —
    done 2026-05-21. The deferred follow-up from closure
    #82 now lands. `prove b.method(args) == k` discharges
    when the method has matching `ensures` clauses.
    - **New rewrite**
      `rewrite_method_calls_to_calls(expr, env, signatures)`
      in [src/checker.rs](src/checker.rs): walks proof
      obligations and fact lists, replaces
      `MethodCall { receiver: Var(name), method, args }`
      with a synthetic `Call { name:
      "<Type>_<method>", args: [receiver, ...args] }`
      whenever the receiver's type resolves to a known
      Struct / Enum (or Ref/RefMut thereof) and the
      mangled function exists in `signatures`. Var-
      receivers only in v1 — chained method calls fall
      through unchanged.
    - **Wired into `prove_with_calls_extra`** as a
      pre-pass before `rewrite_calls_to_fresh_vars` so
      methods reach the existing inline-call discharger.
    - **Bug fix discovered along the way**: the
      extra_facts emitted by inline-call substitution
      (e.g. `__call_0 == self.v * 2` with `self → b`
      → `__call_0 == b.v * 2`) contain field accesses
      that need the closure-#82 struct-field rewrite
      applied AFTER the inline-call pass. Added a
      second sweep of `rewrite_struct_field_accesses`
      over extra_facts before merging into
      rewritten_facts.
    - **Tests**: 2 new lib tests pin single and
      multiple-method discharge.
      747 → 749 lib tests; 47 e2e tests stay green.

83. ~~**Devanagari keyword aliases — Sanskrit / Hindi / Marathi
    (MVP)**~~ — done 2026-05-21. Multi-session item #9
    landed first cut. Programs can be written using
    Devanagari verbs for the structural keywords; the
    lexer routes aliases to the existing English
    `TokenKind` so the parser / checker / IR / backends
    never see Devanagari text. Mixed-script source is
    supported by design.
    - **Lexer extension** in [src/lexer.rs](src/lexer.rs):
      added a non-ASCII byte arm in the main dispatch
      (`b if b >= 0x80`) routing to a new
      `lex_unicode_ident` method. The new method consumes
      ASCII identifier characters plus any non-ASCII byte
      (which by valid-UTF-8 source invariant is part of
      another codepoint), then matches the resulting
      string against the Devanagari keyword-alias table.
    - **Alias table** (`devanagari_keyword`): first cut
      covers ~30 word-level aliases for fn / let /
      return / if / else / while / for / then / ref /
      mut / match / assert / prove / requires / ensures /
      true / false / print / pure / struct / enum /
      const, drawing from Sanskrit (`कार्य फलन माना
      पुनरागम यदि अन्यथा यावत् प्रति तदा …`), Hindi
      (`फलन मानो लौटाओ अगर नहीं तो जबतक के लिए तो …`),
      and Marathi (`कार्य मान परत जर नाहीतर जोपर्यंत
      साठी तर …`). Conflicts resolved in favor of the
      most idiomatic single-word form. Multi-word
      aliases (`नहीं तो`, `के लिए`) deferred — the
      lexer would need lookahead-over-whitespace
      machinery.
    - **Devanagari-named identifiers** also work — the
      table-miss fallback emits `Ident(text)` so
      `let नाम: i64 = 42;` lexes as expected.
    - **4 lib tests** pin the surface:
      `devanagari_keyword_aliases_compile_hindi`,
      `…_sanskrit`,
      `devanagari_aliases_mix_with_english_freely`,
      `devanagari_identifier_names_compile`.
    - **3 example files** demonstrate each language:
      [examples/hindi_keywords.intent](examples/hindi_keywords.intent)
      (arithmetic + asserts), [examples/sanskrit_keywords.intent](examples/sanskrit_keywords.intent)
      (`शुद्ध फलन` abs with `अपेक्षित` / `निश्चित` +
      SMT discharge), and [examples/marathi_keywords.intent](examples/marathi_keywords.intent)
      (recursive factorial with `जर` / `नाहीतर`).
      All three run end-to-end and print expected
      outputs.
    - **Deferred (9a–9f sub-items)**: grammar-consultant
      review of the alias choices (the v1 table is a
      starting point), multi-word aliases via lexer
      lookahead, script-aware diagnostics (errors in
      the language the source uses), and per-language
      README documentation finalization.
    743 → 747 lib tests; 47 e2e tests stay green.

82. ~~**SMT modeling: if-expr, match, struct field access**~~
    — done 2026-05-21. Second multi-session item from
    the queue closed. Three previously-bailing
    constructs now reach the Z3 layer.
    - **If-expressions in SMT** — encode as
      `(ite cond then else)` in
      [smt.rs encode_expr](src/smt.rs). Both branches
      already unified by the checker; target_int
      propagates uniformly. `prove (if x > 0 { x }
      else { 0 - x }) > 0` discharges.
    - **If-expression constant-fold at type-check** —
      when cond is a known bool, collapse to the
      selected branch's constant so `let r = if true
      { 10 } else { 20 };` propagates `r == 10`
      through the binding's constant tracker. Lets
      downstream proofs discharge via constant-fold
      without round-tripping to SMT.
    - **Match expressions in SMT** — encode integer
      patterns as a chain of `(ite (= scrutinee N)
      body …)` ending in the wildcard arm's body.
      Variant patterns deferred (would need EnumInfo
      plumbed through to the SMT layer). Match without
      a wildcard bails (would be partial).
    - **Match constant-fold at type-check** — when
      scrutinee is a known integer, select the matching
      arm (or wildcard) and propagate that arm's body
      constant.
    - **Struct field access in SMT** — for every
      binding initialized via `P { x: e1, y: e2 }`,
      `VarInfo` now carries `struct_literal_fields:
      Option<Vec<(String, Expr)>>`. Before each SMT
      query, `prove_with_calls_extra` synthesizes
      `<name>__<field>` SMT vars and asserts
      `<name>__<field> == encode(field_expr)`. A new
      `rewrite_struct_field_accesses` pass replaces
      `Var(name).field` in the proof expression with
      `Var("name__field")` so the encoder reaches the
      synthesized vars. Only integer / bool fields
      modeled in v1 — Vec / Array / nested struct
      fields skipped.
    - **Method calls in SMT** — deferred. Would need
      MethodCall→Call rewrite before the inline-call
      discharger sees them (the existing
      `rewrite_calls_to_fresh_vars` only handles
      `ExprKind::Call`). Tracked as a follow-up.
    - **Tests**: 7 new lib tests pin the surfaces —
      `smt_if_expression_discharges_in_prove`,
      `smt_if_expression_constant_folds_through_let`,
      `smt_match_expression_discharges_in_prove`,
      `smt_match_constant_folds_through_let`,
      `smt_struct_field_access_discharges`,
      `smt_struct_field_with_computed_init_discharges`,
      `smt_struct_field_disproof_surfaces_counterexample`.
    736 → 743 lib tests; 47 e2e tests stay green.

81. ~~**Block expressions MVP — `let r = { stmts; tail };`**~~
    — done 2026-05-21. First multi-session item from
    the queue closed. Adds a new primary expression form
    that lets `let` initializers (and any expression
    position) bundle a short chain of bindings plus a
    tail value.
    - **AST + IR**: added `ExprKind::Block { stmts, tail
      }` and `TypedExprKind::Block { stmts, tail }`.
      Stmt and Expr were already mutually recursive so
      the Box-wrapped tail is benign.
    - **Parser**: new `TokenKind::LBrace` arm in
      [parse_primary_expr](src/parser.rs#L1931): consume
      leading `let` stmts then a tail expression, expect
      `}`. The `parse_stmt` LBrace arm (closure #77's
      "bare blocks rejected" diagnostic) still fires for
      `{ … }` at statement position — the two arms don't
      overlap since they're reached via different entry
      points.
    - **Checker**: new
      [ExprKind::Block arm in check_expr](src/checker.rs)
      pushes a fresh scope, type-checks each Let RHS,
      binds the name with VarInfo, accumulates
      `TypedStmt::Let`s, then type-checks the tail. The
      block's type is the tail's type. Non-`let` stmts
      surface "block expressions in v1 only allow `let`
      bindings before the tail expression" — keeps the
      MVP analysis simple (no nested control flow inside
      block-expressions).
    - **All checker walks** (substitute_expr,
      expr_mentions, pin_var_to_version,
      pretty_expr, walk_branch_mutations_in_expr,
      walk_expr-for-effects, typed_to_expr) now have
      Block arms that recurse into block-internal let
      RHSes and the tail.
    - **SSA lowering**: bails with `LowerError` so
      programs containing block-expressions
      automatically fall back to the tree backends —
      the SSA-path gate has been updated to declare
      Block as unsupported (`expr_ssa_supported` in
      main.rs).
    - **Tree-LLVM emit**: new arm in
      [emit_expr](src/backend_llvm.rs#L3199) inlines
      each Let as `alloca` + `store` + ctx.locals
      insert, emits the tail, then restores
      outer-scope ctx.locals entries so the
      block-local names don't leak. Capture analysis
      (`walk_expr` for parallel-for / task outlining)
      uses a cloned `declared` set so block-local
      names extend the visible scope without leaking.
    - **Tree-C emit**: new arm uses GCC
      statement-expressions `({ T name = expr; …;
      tail; })` — matches the existing match-emission
      pattern.
    - **SMT**: rejects Block as "block expressions
      not supported in SMT v1" (same posture as
      match, method calls, if-expr).
    - **Format**: pretty-prints inline as `{ stmt;
      stmt; tail }`.
    - **5 lib tests** pin the surfaces:
      `block_expression_with_lets_then_tail_compiles`,
      `nested_block_expression_compiles`,
      `empty_block_expression_just_value_compiles`,
      `block_expression_only_allows_let_inside`,
      `block_expression_shadowing_is_local`.
    - **Example**:
      [examples/block_expressions.intent](examples/block_expressions.intent)
      demonstrates single-line, multi-line, nested,
      and shadowing forms. `cargo run -- run
      examples/block_expressions.intent` exits 0 with
      `r= 15  area= 42  combined= 13  twice_x= 200`.
    - **Unblocks (small items now also done)**: while
      bare `{ … }` as a *statement* still rejects with
      a workaround diagnostic (would need
      `Stmt::Block`, a sibling unit of work), the
      *expression* form is now first-class.
    731 → 736 lib tests; 47 e2e tests stay green.

80. ~~**`clone_at` on `Vec<Struct>` fix in tree-LLVM +
    3 composition probes**~~ — done 2026-05-21. Real
    codegen bug closed.
    - **Root cause:** tree-LLVM's call dispatcher in
      [backend_llvm.rs:2676](src/backend_llvm.rs#L2676)
      handled `vec` / `push` / `set` / `clone` as
      builtins but had no `clone_at` arm. Calls to
      `clone_at` fell through to the generic
      `@fn_<name>` call path, emitting a reference to
      `@fn_clone_at` which was never defined. The
      SSA-LLVM backend had a working
      `clone_at` arm at
      [ssa_backend_llvm.rs:3431](src/ssa_backend_llvm.rs#L3431),
      and `Vec<Vec<…>>` programs happened to stay on
      the SSA path so they worked end-to-end. But
      `Vec<Struct>` programs fell back to tree-LLVM
      and hit the undefined-symbol error at `lli`
      load time. The existing
      `clone_at_extracts_owned_copy_of_inner_vec`
      lib test only checked compile-time success, not
      end-to-end run, which hid the gap.
    - **Fix:** ported the SSA-LLVM arm into tree-LLVM.
      `clone_at(xs, i)` GEPs into the slot, then for
      `is_copy()` elements loads the value directly;
      for `Vec<U>` elements loads then routes through
      the inner Vec's `__clone` helper. Mirrors the
      tree-C `c_element_deep_clone` recursion and the
      SSA-LLVM logic.
    - **Probes confirmed working** (each pinned):
      `clone_at` of `Vec<P>` returning a struct
      slot copy, match returning struct via arms, pure
      fn calling other pure fn, for-loop with mixed
      continue + break.
    Four new lib tests pin the surfaces. 727 → 731
    lib tests; 47 e2e tests stay green.

79. ~~**Twenty-first sweep: 100% pass rate confirms
    small-item saturation; 10 composition pins**~~ —
    done 2026-05-21. First clean sweep — no new bugs,
    no diagnostic issues across 15 probes targeting
    auto-ref/auto-mut-ref dispatch, Vec-of-Vec, mixed
    signed/unsigned arithmetic, escape sequences in
    strings, Str return position, Vec-reassign-in-loop,
    method-chain-on-if-expr-result, and strict
    `ensures` discharge. Ten new lib tests pin the
    cleanest probes. 717 → 727 lib tests; 47 e2e tests
    stay green.

78. ~~**Twentieth sweep: methods-without-self rejected
    + 6 composition pins**~~ — done 2026-05-21. One
    real surface gap closed.
    - **Gap: `methods on T { fn no_self() … }` was an
      orphan** — the hoister
      ([checker.rs:1346](src/checker.rs#L1346)) silently
      renamed it to `T_no_self` but no caller path
      reached the mangled name (`Type.method()` syntax
      doesn't exist, `recv.method()` requires self,
      free-function call `no_self()` finds nothing).
      Probed via `B.make()` — the diagnostic said
      "unknown variable 'B'", which was misleading.
    - **Fix:** in `hoist_methods_into_functions`, check
      that the first parameter is named `self` before
      hoisting. If not, emit "method 'T::name' must
      take `self` as its first parameter — use a free
      function for type-associated helpers without a
      receiver" and skip the mangled-name hoist.
    - **Probes confirmed working** (each pinned):
      `global const` in struct field init, match on
      `u32` scrutinee, `-5` as pattern, `while` with
      `&&` compound cond, multi-item `print` with
      labels, `atomic_compare_exchange` + `atomic_load`
      via correct names.
    Seven new lib tests pin the surfaces. 710 → 717
    lib tests; 47 e2e tests stay green.

77. ~~**Nineteenth sweep: bare-block diagnostic +
    9 composition probes**~~ — done 2026-05-21.
    - **Gap: bare `{ … }` as a statement surfaced
      opaque "expected statement"** —  the parser's
      stmt entry had no LBrace arm, so a free-standing
      block hit the fall-through error with no
      workaround.
    - **Fix:** added an LBrace arm in
      [parse_stmt](src/parser.rs#L894) that emits
      "bare blocks `{ … }` as statements aren't
      supported in v1 — wrap in `if true { … }` for an
      explicit nested scope, or inline the contents".
      `Stmt::Block` would be a larger addition tracked
      under the block-expressions TODO.
    - **Probes confirmed working** (each pinned):
      empty `vec()`, method calling another method on
      self (`self.dbl()` inside `self.quad`), fn
      returning `Vec`, `let (a, b)` destructure of 2
      and 3 names, tuple swap via destructure (`let
      (c, d) = (b, a)`), `set(xs, i, v)` returns
      updated Vec.
    - **v1 limitations pinned:** array types in
      return position (clean diagnostic).
    Nine new lib tests pin the surfaces. 701 → 710
    lib tests; 47 e2e tests stay green.

76. ~~**Seventeenth + eighteenth sweep merged: compile-
    time short-circuit fix + 13 composition probes**~~
    — done 2026-05-21. One real correctness bug closed,
    two more sweeps confirmed no further surface gaps.
    Combined as one effort.
    - **Bug: `false && (provably-bad)` and `true ||
      (provably-bad)` errored at compile time** —
      [check_binary](src/checker.rs#L5275) checked both
      operands unconditionally, so the RHS's const-fold
      (e.g. `10 / x` with `x == 0`) raised "division
      by zero" even when the LHS already determined the
      result. Surfaced spuriously on patterns like `if
      false && debug_check { … }` — false-gated dead
      code that the runtime would never execute.
    - **Fix:** added a short-circuit branch in
      `check_binary`: if op is `&&` / `||` AND the LHS
      const-folds to a value that determines the result
      (`false &&` or `true ||`), the RHS is still
      type-checked (so the AST stays well-formed) but
      its diagnostics are routed to a throwaway Vec.
      Honors runtime semantics. **Regression-guarded**:
      `false || (1 / 0)` correctly still errors —
      that's NOT dead, the result depends on the RHS.
    - **Probes confirmed working** in the merged sweep:
      many fn params (6), deeply nested if (5-level),
      long else-if chain, 5-method chain, match with
      10 arms, ref-param-only collection sum,
      fn-returning-fn-ptr, assert true at compile,
      prove via invariant, for with complex body,
      string `<` compare, OwnedStr `==` Str,
      nested type alias, 3-deep nested for,
      method-call results as fn args, mut-ref struct
      param mutating caller binding, match inside if
      branch, if-expression inside match arm,
      vec_from_for_loop_pop, method_on_method_result,
      ref_to_struct.
    Eight new lib tests pin the surfaces (3 for the
    short-circuit fix + regression guard, 5 for new
    compositions). 693 → 701 lib tests; 47 e2e tests
    stay green.

75. ~~**Sixteenth sweep: discarded call/method-call as
    statement + 6 probe pins**~~ — done 2026-05-21.
    One real surface gap closed and 5 working /
    rejection probes pinned. Each batched as one
    effort.
    - **Gap: `x.bump();` as a discarded statement
      surfaced "expected statement"** — the parser
      had no expression-as-statement fallback, so
      side-effect-bearing mut-ref method calls and
      plain function calls forced users to write
      `let _ = …;`. The diagnostic was opaque too —
      "expected statement" didn't hint at the
      workaround.
    - **Fix:** added a last-chance fallback at the
      end of [parse_stmt](src/parser.rs#L894): if the
      previous arms didn't match, attempt
      `parse_expr()` followed by `;`. If both succeed
      AND the parsed expression is `Call { .. }` or
      `MethodCall { .. }` (gated so bare-`x;` keeps
      surfacing the typo-friendly original error),
      synthesize a `Stmt::Let { name: "_", expr,
      annotation: None, … }` — the existing `let _ =
      …;` desugar handles the rest of the pipeline
      (drop-coverage for Copy results, SSA lowering,
      backend emit). Pos is restored on failure so
      the original error keeps its position.
    - **Confirmed working** (each pinned as a lib test):
      - **Discarded method call statement**
        (`x.bump(); x.bump(); return x.bump();` over
        a `mut ref V` method) — exit 3.
      - **Discarded plain function call statement**
        (`tally(5); tally(10); return tally(20);`).
      - **Bare variable as stmt still rejected**
        (`x;` after `let x = 5`) — gate held.
      - **`print f32`** — outputs decimal.
    - **Confirmed correctly rejected with sharp
      diagnostics** (each pinned):
      - **`i8(-1) as u8`** at compile time — const
        fold catches "cannot be represented as u8".
      - **`10 / z` with `z = 0`** — const fold
        catches "division by zero in constant
        expression".
      - **`let x: u8 = -5`** — same representation
        diagnostic.
    Seven new lib tests pin the surfaces. 686 → 693
    lib tests; 47 e2e tests stay green.

74. ~~**Sharper diagnostics for aggregate `==` / `!=`**~~
    — done 2026-05-21. Replaces the misleading "left
    operand must be numeric, got P" message that was
    showing up for struct / tuple / enum equality.
    - **Root cause:** `check_equality` in
      [src/checker.rs:5651](src/checker.rs#L5651)
      handled bool and Str/OwnedStr equality explicitly,
      but fell through to `promoted_numeric_type` for
      everything else. That function's "must be numeric"
      diagnostic was technically true but unhelpful —
      it didn't say what alternative was available or
      that user-defined equality is forthcoming.
    - **Fix:** added an aggregate-type early-return in
      `check_equality` that emits one of three targeted
      messages:
      - **Struct:** "struct 'P' has no built-in `==` —
        compare field-by-field, or wait for user-defined
        equality (T1.5 phase 2)"
      - **Tuple:** "tuples have no built-in `==` —
        compare each component via `.0` / `.1` / …"
      - **Enum:** "enum 'E' has no built-in `==` — use
        `match` to discriminate, or compare the integer
        tag via `as i64`"
    - **Tests:** existing `struct_equality_rejected`
      test was renamed to
      `struct_equality_rejected_with_targeted_diagnostic`
      and tightened to assert the new specific message.
      Added two parallel tests for tuple and enum.
      684 → 686 lib tests; 47 e2e tests stay green.

73. ~~**Fifteenth sweep: 11 composition probes, 1 v1
    limitation pinned, no codegen bugs**~~ — done
    2026-05-21. Stability sign: no new panics or
    miscompiles surfaced this round. Each probe
    pinned as a lib test.
    Probes confirmed working:
    - **Shadow of fn param via inner `let`** — exit 99.
    - **Shadow of loop counter inside `for` body** —
      `let i = 99` inside `for i from 0 to 3 { … }`
      rebinds in body scope only.
    - **Truncating int cast** (`100000 as i32 as i64`)
      — bit-truncates then sign-extends.
    - **Method chain of three calls** (`c.inc().inc()
      .inc().v`) — exit 3.
    - **Method takes foreign-struct param** (method on
      P accepts Q) — exit 7.
    - **`if`-expression as fn argument** —
      `id(if true { 42 } else { 0 })` exit 42.
    - **`ensures` referencing both a const and a fn
      param** (`ensures _return == x + K;`) — verifier
      discharges. Confirms `;`-terminator on each
      ensures clause before the body brace.
    Probes confirmed correctly rejected:
    - **Match with duplicate `_` arm** — checker flags
      the second wildcard as unreachable.
    Limitations pinned:
    - **`const N` cannot be used as array length**
      (`[i64; N]`) — parser requires an integer
      literal for the length slot. v1 limitation,
      worth deferring until block-expressions or
      compile-time evaluation make it cheap.
    Nine new lib tests pin the surfaces. 675 → 684
    lib tests; 47 e2e tests stay green.

72. ~~**Fourteenth sweep: print-of-aggregate panic fix +
    7 composition probes**~~ — done 2026-05-21. Three
    related real bugs closed in one fix; plus probes
    pinned diagnostics for several rejected surfaces
    and confirmed two working compositions.
    - **Bug: `print` of struct / tuple / enum panicked
      in [backend_llvm.rs:3347](src/backend_llvm.rs#L3347)**
      with "checker rejects print of X but the LLVM
      backend was asked to lower it" — the backend's
      `unreachable!` comment assumed the checker
      rejected non-scalar print items, but the checker
      only rejected `is_array()` / `is_vec()` — not
      structs / tuples / enums.
    - **Fix:** extended the `Stmt::Print` arm in
      [src/checker.rs:2497](src/checker.rs#L2497) to
      also reject `Type::Struct(_)`, `Type::Tuple(_)`,
      and `Type::Enum(_)` with targeted diagnostics
      ("print individual fields", "use `.0` / `.1`",
      "use `match` to convert to int/string"). All
      three previously-panicking probes now surface
      clean diagnostics instead of crashing.
    - **Probes confirmed correctly rejected** (each
      pinned as a lib test):
      - **Match on bool with `true`/`false` patterns**
        — patterns accept ints/variants/`_` only.
      - **Struct equality `a == b`** — no user-defined
        Eq interface in v1.
    - **Probes confirmed working** (each pinned as a
      lib test):
      - **Type alias used as fn param** (`fn flip(c:
        Coord) -> Coord` over `type Coord = (i64,
        i64)`) — exit 3.
      - **Array literal in let-binding** (`let xs:
        [i64; 3] = [10, 20, 30]`) — exit 20.
    Seven new lib tests pin the surfaces (3 panic
    fixes, 2 rejection-diagnostic pins, 2 working
    compositions). 668 → 675 lib tests; 47 e2e tests
    stay green.

71. ~~**Inner-`let` shadow leak fix in SSA `lower_if`**~~ —
    done 2026-05-21. Real correctness bug closed
    (surfaced in closure #69, fixed here).
    - **Root cause:** the SSA lowerer's `Locals` is a
      flat `BTreeMap<String, ValueId>`. When an inner-
      scope `let x` shadows an outer `let x`, both arms
      of `lower_if` would write the inner ValueId into
      the cloned `then_locals`/`else_locals`. The merge
      loop at
      [src/ssa.rs:1537-1543](src/ssa.rs#L1537-L1543)
      then saw the inner value differing from the entry
      value and wired it through a phi, leaking the
      shadow value out to the outer scope. Same-type
      shadow returned the wrong value at runtime;
      cross-type shadow (`let x: bool` over `let x:
      i64`) hit a related LLVM phi typing crash (i1
      mixed with i64).
    - **Fix:** `lower_stmts` now returns
      `Vec<String>` — the names introduced via
      top-level `let` in the stmt list. `lower_if`
      iterates the returned vec for each branch and,
      for any name that ALSO existed in `entry_locals`
      (i.e., the inner `let` shadowed an outer binding),
      pins `branch_locals[name]` back to the entry
      value before the merge. Inner-scope new bindings
      (names not present at entry) continue to be
      ignored by the merge naturally — the merge loop
      only iterates `entry_locals`.
    - **Genuine `=` reassign** (without `let`) still
      flows through correctly: those don't appear in
      the returned let-introduced vec, so the merge
      sees the actual mutation and emits the phi.
    - **Loop bodies** (`lower_while`, `lower_for_iter`)
      were already safe — the carry computation is
      based on `collect_branch_mutations` over the
      typed AST and excludes inner-scope `let`-shadow
      names. Verified by probe.
    - **Tests:** 4 new lib tests pin the surface —
      `inner_let_shadow_does_not_leak_to_outer`
      (same-type), `inner_let_shadow_cross_type_no_phi_error`
      (cross-type), `inner_assign_without_let_still_propagates_to_outer`
      (regression guard for genuine reassign),
      `nested_if_shadows_do_not_leak` (multi-level).
      Known-issue entry deleted from STATUS.md.
      664 → 668 lib tests; 47 e2e tests stay green.

70. ~~**Thirteenth sweep: 11 composition probes batched
    as one effort**~~ — done 2026-05-21. No codegen
    fixes needed; all probes either worked or surfaced
    expected diagnostics. Each pinned as a lib test:
    - **Negative integer literal** (`let x: i64 = -5`)
      — exit 5.
    - **For-loop empty range** (`for i from 5 to 5`) —
      body never executes, exit 0.
    - **For-loop reverse range** (`for i from 5 to 3`)
      — body never executes, exit 0.
    - **`bool as i64` rejected** with clean "cannot
      cast bool to i64" diagnostic — bool/int are
      separate semantic domains; forces explicit
      if/else conversion.
    - **`i64 as bool` rejected** (already pinned by
      existing test; not duplicated).
    - **Match arm with bool literal** (`5 then true,
      _ then false`) — exit 100.
    - **Multiple consts of same type** (3 i64 consts
      summed in main) — exit 6.
    - **Tuple stored as struct field** (`Pair { t:
      (i64, i64) }`) — `p.t.0 + p.t.1` exits 3.
    - **Const used in arithmetic expression** in fn
      body — confirms the literal-only init
      restriction applies only to const decls; uses in
      expression position work normally. Exit 15.
    - **Method call on fn result** (`make().get()`) —
      exit 42. Self-receiver requires explicit
      `self: Type` (keyword-first design).
    - **Match on fn call scrutinee** (`match pick()
      { … }`) — exit 20.
    - **Implicit self method receiver rejected**
      — surfaced incidentally while probing
      `make().get()`; `fn get(self)` without type
      annotation is cleanly rejected.
    Eleven new lib tests pin the surfaces. 653 → 664
    lib tests; 47 e2e tests stay green.

69. ~~**Twelfth sweep: 7 composition probes + shadow-leak
    bug surfaced and recorded as known issue**~~ —
    done 2026-05-21. No codegen fixes landed this turn;
    the sweep pinned clean diagnostics + working
    compositions and surfaced one real bug for a future
    closure.
    Composition probes (each pinned as a lib test):
    - **Generic call site** (`id(5)` against
      `fn id<T>(x: T) -> T`) surfaces a clean
      "monomorphization still in progress" gate
      diagnostic — confirms T1.4 phase 2 gate text.
    - **Payloaded enum variant** (`enum Opt { Some(i64), None }`)
      surfaces a clean "variant has a payload —
      tagged-union codegen still in progress" gate —
      confirms T1.3 phase 2b gate text.
    - **OwnedStr concat chain** (3 `+`s, each binding
      consumed by the next) — exit 0.
    - **Nested match in match-arm expression**
      (`Side.L then (match n { … })`) — exit 30.
    - **`&xs` borrow in for-iter** (`for x in &xs`)
      cleanly rejected with the "use `ref xs`" T0.0
      diagnostic — confirms keyword-first stance.
    - **`&xs` borrow in `clone_at`** rejected with the
      same T0.0 diagnostic.
    - **`&self` method receiver** rejected — keyword-
      first stance applies to method receivers too
      (`fn m(self)` / `fn m(ref self)`).
    Surfaced bug (recorded in STATUS.md known issues —
    not fixed this turn because the fix needs
    scope-stacked `Locals` across `lower_if` /
    `lower_while` / `lower_for_iter`, multi-session):
    - **Inner-scope `let` shadowing leaks to outer
      scope at runtime.** Reproducer: `let x: i64 = 5;
      if true { let x: i64 = 10; } return x;` exits 10,
      not 5. Root cause: SSA lowering's `Locals` is a
      flat `BTreeMap<String, ValueId>`; the inner `let
      x` overwrites the outer entry in the cloned
      `then_locals`, and the if-merge logic at
      [src/ssa.rs:1537-1543](src/ssa.rs#L1537-L1543)
      treats the new ValueId as a mutation of the
      outer binding (wires it through the merge phi).
      Cross-type shadowing (`let x: bool` over `let x:
      i64`) hits a related LLVM-typing bug — the phi
      mixes `i1` with `i64`. Existing compile-time
      test `let_in_branch_can_shadow_outer_with_different_type`
      only checked the type-checker accepts it, not
      runtime semantics. Workaround documented in
      STATUS.md.
    Seven new lib tests pin the surfaces. 646 → 653 lib
    tests; 47 e2e tests stay green.

68. ~~**Eleventh sweep: ArrayLit-as-fn-arg LLVM fix
    + 8 composition probes batched as one effort**~~ —
    done 2026-05-21. One real codegen fix:
    - **ArrayLit as direct function argument**:
      `sum_pts([P{1,2}, P{3,4}, P{5,6}])` panicked at
      `backend_llvm.rs:3085` with "TypedExprKind not
      lowered as standalone expression: ArrayLit
      { … }". The tree-LLVM backend supported
      `ArrayLit` only in let-binding RHS position; when
      passed inline as a call arg it fell through to
      the catch-all unreachable. Replaced the
      `kind => unreachable!(...)` arm in `emit_expr`
      with an `ArrayLit { elements }` arm that
      `alloca`s the `[N x T]` array, GEPs each
      element index, stores the lowered sub-expression,
      then `load`s the whole aggregate so the SSA
      value passed onward has array-by-value semantics
      (matches the let-binding shape). Verified
      `sum_pts([P{1,2}, P{3,4}, P{5,6}])` returns
      `1 + 4 + 5 = 10`.
    Plus 8 composition probes (no fixes needed):
    - **Mutual recursion** (`is_even`/`is_odd`
      bouncing through `n - 1`) — exit 100.
    - **Bool from comparison stored in let**
      (`let big: bool = 100 > 50`) — exit 1.
    - **Function returning enum** + match on the
      result — exit 2.
    - **While invariant referencing const**
      (`while i < N invariant i <= N`) — exit 45.
    - **Print `f32` typed value** — outputs
      `x= 1.500000`.
    - **Match on `i8` scrutinee** with integer
      patterns + wildcard — exit 99.
    - **Match on `u8` scrutinee** (`200u8`) with
      multiple integer patterns — exit 3.
    - **Const initialized with arithmetic over
      another const** correctly rejected
      ("const initializer must be a literal") —
      confirms v1 limitation.
    Nine new lib tests pin the surfaces (one for the
    fix, eight for the probes). 637 → 646 lib tests;
    47 e2e tests stay green.

67. ~~**Tenth sweep: float negation SSA fix + 10
    composition probes batched as one effort**~~ —
    done 2026-05-21. One real codegen fix:
    - **Float negation in SSA-LLVM backend**:
      `let y: f64 = -x;` previously emitted
      `sub double 0, %v` — invalid LLVM IR (the
      integer `sub` instruction rejects float
      operands with "integer constant must have
      integer type"). Fixed by dispatching on the
      operand's type in the UnaryOp::Neg arm and
      emitting `fsub double 0.0, %v` for floats while
      keeping integer `sub 0, %v` for ints. The
      tree-LLVM backend was unaffected (it routes
      Neg through the float-binary dispatcher).
    Plus 10 composition probes (no fixes needed):
    - Bit shifts (`<<` and `>>`) — exit 20.
    - Float arithmetic (`*` `+` `-`) — exit 26.
    - String concat with `+` (`"hello, " + "world"`).
    - String equality (`a == b` on Str type).
    - f32→f64 cast.
    - `len(vec(1,2,3,4,5))` inline — exit 5.
    - Type alias to tuple + field access via `.0`/`.1`
      (`Coord = (i64, i64)`) — exit 18.
    - Match with only wildcard arm — exit 99.
    - `assert` with message string.
    - Boolean short-circuit (`a && (b || true)`).
    Eleven new lib tests pin the surfaces. 626 →
    637 lib tests; 47 e2e tests stay green.

66. ~~**Ninth sweep: 11 composition probes batched
    as one effort**~~ — done 2026-05-21. All probes
    work / behave as expected without code changes:
    - **Pure→pure call chain** (`quad(3)` via
      `double(double(x))`) — exit 12.
    - **Pure→impure call rejected** with clear
      "pure fn 'p' cannot call non-pure function"
      diagnostic.
    - **No-param fn returning bool** + use in if-cond
      — exit 1.
    - **Bool `==`** comparison — exit 0
      (`true == false`).
    - **Float `==`** comparison — exit 100.
    - **Nested for loops** (3×3 inner-body count) —
      exit 9.
    - **While inside for** (3 outer × 2 inner) —
      exit 6.
    - **Boolean negation** (`if !x { 100 } else { 200 }`)
      — exit 200.
    - **Bitwise ops on i64** (`& | ^`) —
      8 + 14 + 6 = 28.
    - **Modulo** (`17 % 5`) — exit 2.
    - **Method call on function result**
      (`make().sum()`) — exit 15.
    Eleven new lib tests pin the surfaces. 615 → 626
    lib tests; 47 e2e tests stay green.

65. ~~**Eighth sweep: 8 composition probes batched
    as one effort**~~ — done 2026-05-21. All probes
    in this batch worked / behaved as expected
    without code changes; lib tests pin each:
    - **Partial enum match + wildcard** (cover 2 of
      4 variants, wildcard catches Yellow) — exit 99.
    - **Multiple `methods on T { … }` blocks for
      same type** — two separate blocks each
      contribute methods; the hoist pass appends to
      the regular function table. Exit 7.
    - **While + break + continue** (skip i=5,
      break at i=8) — exit 23 = 1+2+3+4+6+7.
    - **Print i8 typed value** (`-5`) — outputs
      `x= -5`.
    - **Const with underscored literal**
      (`1_000_000`) — exit 232 (1000 & 0xFF).
    - **Empty array literal** correctly rejected
      with clean "empty array literals are not
      supported" diagnostic.
    - **Single-element Vec** (`vec(42)`) — exit 42.
    - **Const as match pattern** correctly rejected
      (patterns accept integer literals + variant
      paths + `_`, not bound identifiers; parser
      expects `.` after the would-be enum name).
    Eight new lib tests pin the surfaces. 607 → 615
    lib tests; 47 e2e tests stay green.

64. ~~**Seventh sweep: bool print SSA gap noted +
    four probes**~~ — done 2026-05-21. Three
    composition probes (no fixes needed):
    - **For-loop with const as upper bound**
      (`for i from 0 to N` where `N: i64 = 5`) —
      exit 10.
    - **Duplicate struct field rejected** —
      "'Bad' has duplicate field 'x'" diagnostic.
    - **Cast i64 → bool rejected** ("cannot cast
      i64 to bool" — booleans must come from comparisons
      or literals, not numeric conversion).
    - **Method called twice on same Copy value**
      (`p.sum() + p.sum()`) — exit 14.
    Documented latent SSA-bool-print gap: bool
    literals printed via the SSA path render as
    `1`/`0` instead of `true`/`false`. The
    underlying issue is that bool literals arrive
    at the print site typed as i64 in the SSA
    value_types map (not Bool), so the Bool arm in
    `intent_print_item` lowering doesn't fire.
    Tree-backend paths (programs with structs/enums/
    match/methods) print correctly. Unifying
    requires SSA-side type-tracking work; not
    fixing this turn. Four new lib tests
    (`for_loop_with_const_as_upper_bound`,
    `duplicate_struct_field_rejected`,
    `cast_int_to_bool_rejected`,
    `method_called_twice_on_same_copy_value`).
    603 → 607 lib tests; 47 e2e tests stay green.

63. ~~**Checker: recursive-struct cycle detection +
    tuple-containing-struct probe**~~ — done
    2026-05-21. One real soundness fix:
    - **Recursive struct detection**: `struct Node {
      val: i64, child: Node }` previously parsed +
      type-checked cleanly because no instance was
      created in `main`, leaving the infinite-size
      bug latent. Added a cycle-detection pass after
      struct-registry building: builds the
      direct-field-type adjacency map (Tuple +
      Array recurse through; Vec/Ref/RefMut/Atomic/
      Mutex/Guard/Channel break the dependency since
      they're pointer-shaped) and DFS-checks each
      struct. Surfaces clear "struct 'X' is recursive
      (directly or transitively) — contains itself
      by value, which has infinite size; use `ref T`
      / `Vec<T>` to break the cycle via the heap"
      diagnostic. Catches both direct
      (`struct A { f: A }`) and mutual
      (`struct A { f: B } struct B { f: A }`) cycles.
    Plus one composition probe (no fix needed):
    - **Tuple containing struct** (`t.0.x` where t is
      `(Point, i64)`) — exit 111 from
      `t.0.x + t.0.y + t.1`.
    Three new lib tests
    (`recursive_struct_rejected`,
    `mutually_recursive_structs_rejected`,
    `tuple_containing_struct_field_access`).
    600 → 603 lib tests; 47 e2e tests stay green.

62. ~~**Parser fix: nested-tuple access `t.0.0` +
    five composition probes**~~ — done 2026-05-21.
    One real parser fix:
    - **Nested tuple access**: `t.0.0` previously
      errored "expected integer (tuple index) or
      identifier (field name) after '.'" because the
      numeric lexer greedily reads `0.0` as a float
      literal — so `t.0.0` tokenized as
      `Ident(t) Dot Float(0.0)`. Added a
      `TokenKind::Float` arm in the postfix-`.` parse:
      uses `format!("{:?}", value)` (round-trippable
      form like `0.0` — `{}` strips trailing-zero
      fractions to `0`) to get the literal text, splits
      on `.`, and if both halves parse cleanly as u32,
      emits two chained TupleAccess nodes. Lets users
      write `((1,2),(3,4)).0.0` without intermediate
      variables. Verified: exit 5 = `t.0.0 + t.1.1`
      on `((1,2),(3,4))`.
    Plus four composition probes (no fixes needed):
    - **Struct field reorder in literal** (`P { y: 4,
      x: 3 }`) — checker reorders to canonical
      declaration order before codegen. Exit 7.
    - **Function result as struct field initializer**
      (`Outer { inner: make_inner(42) }`) — exit 42.
    - **`let _: i64 = side_effect();`** discard
      pattern — side-effect runs, value ignored.
    - **Const-to-const comparison** (`if A < B { … }`)
      — exit 100.
    Six new lib tests
    (`nested_tuple_access_double_dot`,
    `struct_field_reorder_in_literal`,
    `function_result_as_struct_field_initializer`,
    `let_underscore_discard_compiles`,
    `const_compared_with_other_const`, plus
    the existing probe-as-test pattern).
    **Crossed 600 lib tests** this turn (started
    session at 467). 595 → 600 lib tests; 47 e2e
    tests stay green.

61. ~~**Sixth sweep: five composition probes around
    methods, prints, bool ops, negative for-bounds,
    match-on-method-result in for-loop**~~ — done
    2026-05-21. All five worked end-to-end without
    code changes:
    - **Print method-call result**: `print "sum=",
      p.sum()` — method call as print item. Outputs
      `sum= 7`.
    - **Nested method call as method arg**:
      `c.add(c.inc())` — exit 21 = 10 + 11.
    - **Boolean ops in struct initializer**:
      `Cfg { enabled: true && false, secure: false ||
      true }` — exit 100.
    - **For-loop with negative bounds**:
      `for i from -5 to -1 { total = total + i; }` —
      exit 242 (256 - 14).
    - **Match on method-call result inside
      for-loop**: most complex composition — for-iter
      over `[Probe; 3]` with `match p.sign() { Sign.Neg
      then -1, Sign.Zero then 0, Sign.Pos then 1 }`
      sums to 0. Tests for-iter × method dispatch ×
      enum match × value-returning match arms in one
      program.
    Five new lib tests
    (`print_method_call_result`,
    `nested_method_call_as_method_arg`,
    `boolean_ops_in_struct_initializer`,
    `for_loop_with_negative_bounds`,
    `match_on_method_result_inside_for_loop`).
    590 → 595 lib tests; 47 e2e tests stay green.

60. ~~**Struct field cap raised 8 → 64 + sweep of
    probes**~~ — done 2026-05-21. Real-world domain
    types (game entities, protocol messages,
    configuration structs) often have 10-30 fields;
    the 1..=8 cap was a v1 over-conservative pick.
    Raised cap to 1..=64 (still flags excess as a
    code-smell signal). Updated `decl.fields.len() >
    8` → `> 64` and the diagnostic text + the AST
    doc comment + the
    `empty_struct_rejected` test's 1..=8 assertion.
    Verified a 15-field Entity struct compiles
    cleanly (returns 150 from
    `e.health + e.shield`). The 65-field rejection
    diagnostic still fires correctly. Note:
    `f32` / `f64` etc. remain lexer keywords so
    can't be used as field names (consistent with
    `i32`/`i64`/`u32`/etc.) — workaround is to use
    different names. Plus three composition probes
    (no fixes needed):
    - Bool const (`const ENABLED: bool = true;`) +
      use in if-cond.
    - Struct with mixed integer widths (u32, u16,
      u8) + method casting each to i64.
    - Method chain starting from a struct literal +
      ending in a field access
      (`Point { x: 1, y: 2 }.moved(10).moved(5).x`).
    Five new lib tests
    (`struct_with_15_fields_compiles_after_cap_raise`,
    `struct_with_65_fields_rejected_after_cap_raise`,
    `bool_const_compiles`,
    `struct_with_mixed_int_sizes`,
    `method_chain_on_struct_literal_into_field`).
    585 → 590 lib tests; 47 e2e tests stay green.

59. ~~**Fifth sweep: trailing comma in generic param
    + where clause + edge-case probes**~~ — done
    2026-05-21. Two real parser gaps fixed:
    - **Trailing comma in generic param def**
      (`fn id<T,>(x: T) -> T { … }`) — added
      break-on-`>`-after-comma in the type-param loop
      so multi-line generic param lists match the
      style used everywhere else.
    - **Trailing comma in where-clause bounds**
      (`where T is Cmp,`) — added break-on-
      `{`/`requires`/`ensures`-after-comma in the
      where-clause loop.
    Plus four composition probes (no fixes needed):
    - Negative-in-range const (`const X: i8 = -100;`)
      compiles cleanly (i8 range -128..=127).
    - Negative-out-of-range const (`const X: i8 =
      -200;`) caught by the new overflow check.
    - Empty struct (`struct Empty { }`) correctly
      rejected by the
      "1..=8 fields" cap diagnostic.
    - Print of bool struct field (`print "flag=", f.on`)
      outputs `true`/`false` correctly.
    - Negative-float const (`const PI_NEG: f64 = -3.14;`)
      compiles via unary-minus literal path.
    Five new lib tests
    (`const_negative_literal_in_range_compiles`,
    `const_negative_literal_out_of_range_rejected`,
    `generic_type_param_trailing_comma_accepted`,
    `where_clause_trailing_comma_accepted`,
    `empty_struct_rejected`).
    580 → 585 lib tests; 47 e2e tests stay green.

58. ~~**Fourth sweep: const overflow check + four
    more composition probes**~~ — done 2026-05-21.
    One real soundness fix:
    - **Const integer literal range check**: was
      previously accepting `const X: i8 = 200;`
      silently — the value-fits-type check wasn't
      called for const decls. Added a
      `value_fits_type(*v, &decl.ty)` guard in the
      const-registry loop with a clear
      "literal N does not fit in T" diagnostic
      pointing at the value's span. Bare-i8/u16/i32
      const overflows now surface at decl time
      instead of silently truncating at codegen.
    Four composition probes (no fixes needed):
    - **Recursive fn calls itself**: `fib(10)` via
      `fib(n-1) + fib(n-2)` — exit 55.
    - **Empty match rejected**: `match c { }` correctly
      surfaces "non-exhaustive match: missing arm for
      'Color.Red'" diagnostic for the first variant.
    - **Match on struct's enum field**:
      `match it.kind { Side.Left then …, … }` —
      exit 206 (-50 truncated to u8).
    - **Bool match patterns** documented as not yet
      supported (use `if b { … } else { … }`).
    Four new lib tests
    (`const_literal_overflow_rejected`,
    `recursive_function_calls_itself`,
    `empty_match_rejected_as_non_exhaustive`,
    `match_on_struct_enum_field`). 576 → 580 lib
    tests; 47 e2e tests stay green.

57. ~~**Third sweep: trailing-comma in fn/interface
    param defs + four more composition probes**~~ —
    done 2026-05-21. One real parser gap found and
    fixed:
    - **Trailing comma in `fn` param def**:
      `fn add3(a: i64, b: i64, c: i64,)` errored
      "expected identifier" at the `)`. Fixed in
      `parse_function` — added the same
      break-on-`)`-after-comma pattern used by
      struct fields / call-args. Sibling fix in
      `parse_interface_decl` so interface method
      sigs also accept trailing commas in their
      param lists.
    Four additional composition probes (all worked
    without code changes):
    - **Const in struct field initializer**
      (`Point { x: ORIGIN, y: ORIGIN + 1 }`) — exit
      201.
    - **Match scrutinee that's a method-call result**
      (`match p.choose() { … }`) — exit 200.
    - **Method returning `Vec<i64>` via if-expression
      branches** (`if self.flag { vec(1,2,3) } else
      { vec(10,20,30) }`) — exit 20.
    - **Const used in array index** (`xs[N as i64 -
      1]`) — exit 3.
    Four new lib tests
    (`function_param_def_trailing_comma_accepted`,
    `const_used_in_struct_field_initializer`,
    `match_scrutinee_is_method_call_result`,
    `method_returns_vec_via_if_expression`).
    572 → 576 lib tests; 47 e2e tests stay green.

56. ~~**Second sweep: six more composition probes,
    all pinned as lib tests**~~ — done 2026-05-21.
    Each probe took <5 minutes; all worked without
    code changes (the underlying machinery already
    composed correctly):
    - Type alias chain of length 4
      (`A→B→C→D→i64`) — the alias resolution pass
      unfolds the chain transitively.
    - Type alias to `Vec<i64>` (`type IntList =
      Vec<i64>;`) — alias substitution recurses
      through `Type::Vec` so the alias works as a
      function return + let annotation.
    - Enum with single variant + match
      (exhaustiveness handles N=1).
    - Struct with `bool` field + if-expression
      (`if self.on { self.val } else { 0 }`).
    - Negative integer literal as struct field
      initializer (`Vec2 { x: -5, y: -3 }`).
    - `assert` accepts a method-call result as the
      predicate (`assert p.sum() == 7;`).
    Six new lib tests
    (`type_alias_chain_of_length_four`,
    `type_alias_to_vec_compiles`,
    `enum_with_single_variant_matches`,
    `struct_with_bool_field_plus_if_expr`,
    `negative_literal_as_struct_field_initializer`,
    `assert_with_method_call_result`). 566 → 572
    lib tests; 47 e2e tests stay green.

55. ~~**Sweep of small composition tests + format
    round-trips**~~ — done 2026-05-21. Sweep of
    bounded items each <5min, executed in sequence:
    - **Struct literal trailing comma** — already
      worked, no change needed (`Point { x: 3, y: 4, }`
      parses cleanly).
    - **Bool method in if-cond**: `if r.contains(15)
      { return 100; }` — works on both backends. One
      new lib test.
    - **Nested mut-ref field assign**:
      `self.i.val = self.i.val + 1;` inside a method
      taking `self: mut ref Outer` (combines the
      nested-field-assign codegen path with the
      mut-ref method shape). Works. One new lib
      test.
    - **For-iter borrow of Vec<Struct> with method
      call in body** — exit 10 from sum of two Point
      sums. Already covered in spirit by an earlier
      test.
    - **Mixed enum + integer patterns in single
      match** correctly rejected with clean
      diagnostic.
    - **Format round-trips**: three new tests pin
      previously-untested format surfaces
      (`field_assign`, `pure fn` method,
      method-call chain). All round-trip cleanly
      through the formatter.
    No code fixes needed — everything worked or had
    been previously verified. Five new lib tests
    (`bool_method_used_in_if_condition`,
    `nested_mut_ref_field_assign_works`,
    `formats_field_assign_round_trip`,
    `formats_pure_fn_method_round_trip`,
    `formats_method_chain_round_trip`). 561 → 566
    lib tests; 47 e2e tests stay green.

54. ~~**Parser: helpful diagnostic for `xs[i].field = …;`
    mixed-place-assign**~~ — done 2026-05-21. v1
    doesn't have a place-tracker codegen that can
    route an indexed-then-fielded lvalue through both
    backends — the parser previously surfaced opaque
    "expected statement" when users tried this
    shape. Added `looks_like_index_then_field_assign`
    that walks
    `<ident>[…].<ident>(.<ident>)* =` and surfaces a
    clean diagnostic + the standard workaround:
    "copy the element to a local, modify a fresh
    struct literal, and write it back". The
    workaround (`let copy = xs[i]; xs[i] = T { f1:
    new, f2: copy.f2, … };`) is verified working
    end-to-end. One new lib test
    (`index_then_field_assign_gated_with_workaround`).
    Lifting the gate is part of T1.2 phase 2b
    (place-tracker codegen for chained-index-and-
    field lvalues). 560 → 561 lib tests; 47 e2e
    tests stay green.

53. ~~**LLVM index-assign of struct value into
    `[Struct; N]` slot**~~ — done 2026-05-21. Probed
    `pts[1] = Point { x: 99, y: 100 };` on a fixed-size
    struct array; the LLVM IndexAssign emit panicked
    on `llvm_type(element)` for the struct element
    type (same root cause as earlier
    [Struct;N] read-path fixes). Switched to
    `llvm_type_string` for the element-type spelling.
    Now works end-to-end on both backends, exit 199 =
    99 + 100. The previous in-place IndexAssign codegen
    machinery for arrays (bounds-checked GEP +
    store) didn't otherwise need changes — only the
    type spelling. One new lib test
    (`index_assign_to_struct_array_element`).
    559 → 560 lib tests; 47 e2e tests stay green.

52. ~~**For-iter over `[Struct; N]` + method call in
    body**~~ — done 2026-05-21. One composition probe
    + one lib test: looping over a fixed-size struct
    array and calling a method on each element.
    Verified end-to-end: 1²+1² + 3²+4² + 5²+12² = 196
    (Pythagorean triple totals). Also probed (no fix
    needed, documented as known SMT gap): while-loop
    invariants combined with method calls / struct
    field reads in the body — the SMT encoder rejects
    method calls and struct field access as
    Unsupported, so the verifier can't reason about
    these loops. The runtime path still works; only
    proof obligations against those expressions fail.
    Same root cause as the previously documented
    SMT-struct gap (#46). 558 → 559 lib tests; 47
    e2e tests stay green.

51. ~~**Struct + method composition with f64 / u8
    field types + nested struct field**~~ — done
    2026-05-21. Three additional non-i64 scalar
    compositions probed:
    - **`struct Vec3 { x: f64, y: f64, z: f64 }`**
      with `dot` method returning f64. Tests that
      method return type can be f64 and that per-name
      struct typedef in both backends handles
      floating-point fields. Exit 34 = 32 (dot
      product) + 2 (scaled.x as i64).
    - **`struct Color { r: u8, g: u8, b: u8 }`** with
      `brightness` method casting each field to i64
      and summing. Tests that smaller integer types as
      fields work end-to-end. Exit 194 (450 & 0xFF
      from the brightness sum).
    - **Method accessing `self.inner.field` through a
      nested struct field**: `o.weighted()` reads
      `self.inner.val * self.scale`. Tests that
      nested struct-field reads through `self` work
      inside a method body. Exit 35 = 5 * 7.
    Two new lib tests
    (`struct_with_float_fields_and_method`,
    `struct_with_u8_fields_and_method`).
    556 → 558 lib tests; 47 e2e tests stay green.

50. ~~**Enum-typed struct field + bool-returning
    method**~~ — done 2026-05-21. Single composition
    probe covering three features intersecting:
    enum value as a struct field type
    (`status: Status` where Status is an enum),
    a method on the wrapping struct that matches on
    that enum field (with wildcard arm), and a method
    returning `bool` (most existing methods tests
    return i64). End-to-end works on both backends.
    Verified the
    `struct name 'Task' is a reserved built-in type`
    guard correctly fired on the first naming attempt
    (Task is the parallel-for handle's built-in
    type) — renamed to `Job`. One new lib test
    (`enum_as_struct_field_plus_method_returning_bool`).
    555 → 556 lib tests; 47 e2e tests stay green.

49. ~~**Match composition: inside mut-ref method body
    + nested inside if-expression branches**~~ — done
    2026-05-21. Two further compositions pinned as
    lib tests:
    - **Match as RHS of field-assign through mut-ref**:
      `self.val = match c { … };` inside a method that
      takes `self: mut ref Counter`. The match emits
      its value into the field-store path cleanly.
    - **Match inside each branch of an if-expression**:
      `if x > 100 { match m { … } } else { match m {
      … } }`. Stresses the LLVM phi-predecessor fix
      from earlier (the inner match introduces basic
      blocks; the outer if-expr's phi must track the
      actual tail BB) in a different containing
      context than the if/else-if case that originally
      triggered the fix. Exit 26 = 1050 & 0xFF (OS
      exit code truncation).
    Two new lib tests
    (`match_in_mut_ref_method_body`,
    `match_nested_in_if_expression_branches`).
    553 → 555 lib tests; 47 e2e tests stay green.

48. ~~**Methods + Vec composition: take ref Vec arg,
    take mut ref Vec, return Vec<Struct>**~~ —
    done 2026-05-21. Three additional method/Vec
    compositions pinned via tests + probes:
    - **Method takes `ref Vec<T>`**: iterate via index
      with `len(xs)` auto-derefing through the ref.
      Exit 115 = (1+2+3+4+5) + 100.
    - **Method takes `mut ref Vec<T>`**: index-assign
      `xs[i] = self.default` through the mut-ref
      borrow. Exit 42 after filling all positions.
    - **Method returns owned `Vec<Struct>`**:
      empty `vec()` + repeated push of Point literals
      + return through the method's affine path.
      Exit 6 = Point{2,4}.x + Point{2,4}.y at
      index 2. Two new lib tests
      (`method_takes_ref_vec_arg`,
      `method_returns_vec_of_struct`). 551 → 553
      lib tests; 47 e2e tests stay green.

47. ~~**Method-to-method calls + recursive methods**~~ —
    done 2026-05-21. Two additional method-related
    compositions pinned as lib tests:
    - **Method calls another method on same type**:
      `self.other_method()` inside a method body. The
      hoisted method's body is checked exactly like
      any free function — calls to other hoisted
      methods resolve through the normal signature
      table. Verified `Point.dist_sq() * self.x` via
      `Point.weighted()` returns 75 from (3,4).
    - **Recursive methods**: factorial-style recursion
      where the method constructs a smaller instance
      of the same struct and calls itself. Verified
      `Counter{n:5}.factorial()` returns 120. The
      recursion uses a Copy struct so no affine /
      lifetime issues arise.
    Also probed: method body calls a free function
    that consumes `Vec<i64>`. Works correctly (exit
    110 = (1+2+3+4) + 100). 549 → 551 lib tests;
    47 e2e tests stay green.

46. ~~**Methods on type alias + composition regression
    extensions + documented SMT-struct gap**~~ —
    done 2026-05-21. Added one new lib test
    (`methods_on_type_alias_to_struct_compiles`)
    verifying that `type Pt = Point;` followed by
    `methods on Pt { … }` works correctly — the
    alias-substitution pass rewrites the methods-block
    target to `Type::Struct("Point")` before the
    hoist runs, so the methods land under the
    `Point_<method>` mangled namespace as expected.
    Verified additional compositions end-to-end
    without code changes:
    - **For-iter over method-returned Vec**:
      `let xs = r.collect(); for v in ref xs { … }`
      returns 10 from sum of [1,2,3,4].
    - **Method taking another struct value**:
      `a.dist_to(b)` returns 25 from
      `(3-0)² + (4-0)²`.
    Documented latent SMT gap: method
    `ensures _return >= self.lo` doesn't carry
    through to the caller because the SMT encoder
    rejects struct-field access (`self.lo`) as
    Unsupported. The runtime path still works — the
    method computes the correct value — but
    callers can't `prove` properties about
    method results that depend on the receiver's
    fields. Tracking as a future enhancement
    alongside T1.3 phase 2b (which would also benefit
    from extended SMT modeling). 548 → 549 lib
    tests; 47 e2e tests stay green.

45. ~~**Composition regression tests pinning struct +
    tuple + method interactions**~~ — done 2026-05-20.
    Probed more feature compositions and added three
    new lib tests to lock in regression protection:
    - **Tuple-typed struct field**:
      `struct Pair { coord: (i64, i64), label: i64 }`.
      The C backend's `c_element_storage` and the LLVM
      backend's `llvm_type_string` both render the
      per-shape tuple struct as the field type;
      access via `p.coord.0` works through both.
    - **Method takes tuple arg + returns tuple +
      destructure-let**:
      `o.shift((10, 20))` returns `(i64, i64)` which
      then destructures with `let (a, b) = …;`.
    - **Empty `Vec<Struct>` + push**: `let xs:
      Vec<Point> = vec();` then `push(xs, Point{…})` —
      tests the no-element initial allocation through
      the struct-sizeof codegen path on both backends.
    Also probed three more compositions end-to-end
    without code changes: complex method using mut-ref
    + if-expression to update fields (counter with
    history-max tracking), tuple field plus arithmetic
    on tuple components, SMT verifier correctly
    catching overflow on `pure fn` with `ensures
    _return >= 0`. 545 → 548 lib tests; 47 e2e tests
    stay green.

44. ~~**Method-call diagnostic: helpful message for
    by-value-self via ref receiver**~~ — done
    2026-05-20. Auto-ref handles `p.method()` when
    receiver is `T` but method takes `self: ref T` —
    the checker wraps the receiver in
    `ExprKind::Ref`. The *inverse* case, where
    receiver is already `ref T` / `mut ref T` but
    method takes `self: T` by value, can't be silently
    coerced because the language doesn't have an
    implicit deref expression (Copy-deref through a
    borrow would need a new ExprKind or
    coerce_checked extension). Previously surfaced
    the cryptic generic
    `argument 1 to 'Point_sum' must be assignable to
    Point, got ref Point` diagnostic. Improved: the
    MethodCall checker now detects this case
    specifically and emits
    `method 'sum' takes \`self: Point\` by value but
    the receiver is a borrow (ref Point); either
    change the method signature to \`self: ref Point\`
    / \`self: mut ref Point\`, or copy the value by
    reconstructing the struct literal before calling`.
    Verified workaround: explicit field-by-field
    struct copy compiles and runs cleanly. One new
    lib test
    (`method_call_on_ref_receiver_value_self_gets_helpful_diagnostic`).
    544 → 545 lib tests; 47 e2e tests stay green.

43. ~~**`min` / `max` made context-sensitive
    identifiers**~~ — done 2026-05-20. Previously
    `min` and `max` were global reserved lexer keywords
    used only by `reduce X with min;` /
    `reduce X with max;` clauses and the
    `min(a, b)` / `max(a, b)` builtin intrinsics —
    but blocked them as struct field / local / param
    names everywhere else in the program. Tripped
    while writing `examples/tracker.intent` which
    naturally wanted `min` and `max` as struct
    fields. Made them context-sensitive: removed
    `"min" => TokenKind::Min` and
    `"max" => TokenKind::Max` from the lexer keyword
    table so they now lex as `Ident("min")` /
    `Ident("max")`. The reduction-op parser arm
    switched from matching `TokenKind::Min` /
    `TokenKind::Max` to matching `Ident` with text
    `"min"` / `"max"`. The `parse_primary_expr` arm
    that special-cased `min(a, b)` / `max(a, b)` as
    intrinsic calls was removed entirely — the regular
    Ident-then-Call path handles them, and the
    checker's existing `name == "min" | "max"`
    dispatch in `check_call` lowers to the
    intrinsic backend. End-to-end verified:
    `examples/tracker.intent` now uses
    `struct Tracker { count: i64, min: i64, max: i64 }`
    cleanly and prints
    `count= 4 min= 5 max= 20 range= 15`. Also added
    a lib test
    (`min_max_are_context_sensitive_identifiers`)
    that pins both surfaces (`min(r.min, 3)` + max
    on the field). The unused `TokenKind::Min` /
    `TokenKind::Max` variants stay in the enum (no
    longer produced by the lexer; the parser arms
    that referenced them are gone). 543 → 544 lib
    tests; 47 e2e tests stay green.

42. ~~**Example: tracker.intent demonstrating struct +
    mut-ref methods + if-expression composition**~~ —
    done 2026-05-20. Wrote a real-world Tracker that
    keeps `count`, `lo`, `hi` Copy-scalar fields and
    a method that takes `self: mut ref Tracker` and
    updates all three using if-expressions
    (`self.lo = if v < self.lo { v } else { self.lo };`).
    Discovered while writing it that `min` and `max`
    are reserved lexer keywords (for reduction ops),
    so renamed to `lo` / `hi`. Also probed two more
    feature compositions:
    - **For-iter over Vec<Struct>** (both `for p in
      ref xs` borrowing and `for p in xs` consuming
      forms) — both work end-to-end on both backends,
      returning 21 from (1+2)+(3+4)+(5+6).
    - **Parallel for with struct array + reduction**
      (`parallel for i from 0 to 4 reduce total with
      +;`) — works end-to-end with `total = total + s`
      shape. Returns 20 from (1+1)+(2+2)+(3+3)+(4+4).
    The tracker example is now part of the e2e
    `intentc test` pass. Prints
    `count= 4 lo= 5 hi= 20 range= 15`. 543 lib tests
    (no new tests; just example file added); 47 e2e
    tests stay green.

41. ~~**Feature-composition probes (no fixes needed,
    just verification)**~~ — done 2026-05-20.
    Confirmed end-to-end correctness across both
    backends for the following compositions that
    previously hadn't been explicitly tested:
    - **If-expression returning a struct value**
      (e.g. `let p = if cond { Point{1,2} } else
      { Point{3,4} };`) — exit 3.
    - **Match expression returning a struct value**
      (4-arm match on a Direction enum, each arm
      constructs a different Point) — exit 1.
    - **Method with nested if-expression returning
      a struct** (4-quadrant origin computation) —
      exit 255 (signed-truncation of -1).
    - **Integer match returning a struct** (lookup
      table pattern, `match key { 1 then Pair{…}, …,
      _ then Pair{…} }`) — exit 70.
    - **Method call as for-loop bound**
      (`for i from 0 to r.size()`) — exit 10.
    - **Methods on built-in types correctly rejected**
      with clean diagnostic (`methods on Atomic<i64>`
      surfaces "target must be a struct or enum type,
      got Atomic<i64>"; `methods on Vec` fails earlier
      in the parser since Vec requires `<T>`).
    - **Method returning ref T** correctly rejected
      with "function return type cannot be a
      reference type" (lifetime tracking gate; phase
      2 work).
    All 29 example files in `examples/*.intent` run
    cleanly end-to-end via the LLVM backend. No
    code changes this turn — purely a verification
    pass to certify the cumulative feature set
    composes correctly under the new
    methods/if-expression/match-pattern surface.

40. ~~**Format: IfExpr span-zeroing in `strip_spans` +
    else-if chain round-trip test**~~ — done
    2026-05-20. Discovered while writing the else-if
    chain round-trip test: the format-roundtrip
    invariant compares ASTs with all spans zeroed, but
    `strip_spans` never recursed into `IfExpr`'s
    cond/then/else sub-expressions, so input spans
    leaked into the comparison. Once the parser re-read
    the formatted output and produced different (but
    structurally identical) spans, the assertion fired
    with no actual structural diff. Fixed by adding an
    IfExpr arm to `zero_expr` that recurses into the
    three sub-expressions. Also verified through
    feature-composition probes that the following all
    work end-to-end on both backends:
    `pure fn` methods,
    methods with `requires` / `ensures` contracts,
    methods returning `Vec<T>`,
    push on `Vec<Struct>`,
    struct-in-struct (`Outer { i: Inner, … }`).
    Methods on tuple aliases stay gated as a v1
    limitation (methods only attach to nominal
    struct/enum types — would need monomorphization +
    type-alias-aware dispatch to lift). One new format
    round-trip test
    (`formats_if_expression_else_if_chain_round_trip`).
    542 → 543 lib tests; 47 e2e tests stay green.

39. ~~**Parser: trailing commas in function-call +
    method-call arg lists**~~ — done 2026-05-20.
    Caught while writing the composite_full demo:
    multi-line `vec(Point{1,2}, Point{3,4},)` rejected
    the trailing comma on the last arg, breaking the
    natural style users already get for struct
    fields / enum variants / methods blocks / array
    literals. Added the same break-on-`)`-after-comma
    pattern to both `parse_primary_expr`'s
    Call-args loop and the MethodCall postfix loop.
    Two new lib tests (`call_args_trailing_comma_accepted`,
    `method_call_trailing_comma_accepted`).
    540 → 542 lib tests; 47 e2e tests stay green.

38. ~~**T1.2 follow-up: Vec<Struct> on the LLVM
    backend**~~ — done 2026-05-20. Four changes:
    - **`vec_struct_tag`**: added `Type::Struct(name)`
      → `Struct_<name>` and `Type::Enum(name)` →
      `Enum_<name>` arms (matches the C backend's
      mangling). Without these, the function fell
      through to `llvm_type(element)` which panics on
      aggregates.
    - **`vec_element_size_expr`**: new function
      returning the LLVM size *expression* for an
      element type. For scalar/array elements it returns
      the static `u64` byte count as a literal. For
      struct/tuple elements it returns the LLVM idiom
      `ptrtoint (T* getelementptr (T, T* null, i32 1)
      to i64)` — a constant expression that LLVM
      resolves to `sizeof(T)` at compile time accounting
      for field alignment.
    - **`emit_vec_helpers`**: switched from
      `vec_element_byte_size(element) as i64` (which
      returns 8 for every aggregate — leading to heap
      under-allocation) to the new size expression.
    - **`emit_vec_let_from_literal`**: emits a runtime
      `mul i64 %count, sizeof_expr` for struct/tuple
      Vec literals instead of computing the byte count
      at compile time.
    - **`emit_expr` Vec-Index path**: uses
      `llvm_type_string(element)` instead of
      `llvm_type(element)` so struct/tuple element
      types render their LLVM spelling.
    Result: `Vec<Point>` now compiles + runs on the
    LLVM backend (matching the C backend that was
    fixed earlier). End-to-end verified via
    `/tmp/vec_of_struct.intent` (returns exit 3 from
    `xs[0].x + xs[0].y` on `vec(Point{1,2},
    Point{3,4})`). Renamed
    `vec_of_struct_compiles_via_c_backend` →
    `vec_of_struct_compiles_on_both_backends`.
    540 lib tests stay at 540 (just renamed); 47 e2e
    tests stay green.

37. ~~**T1.2 follow-up: [Struct;N] arrays on the LLVM
    backend + for-iter over struct arrays**~~ — done
    2026-05-20. Three related LLVM panic-fixes (each at
    the same root cause: `llvm_type(Type::Struct(_))`
    panics, callers must use `llvm_type_string`):
    - **`llvm_type_string` for Array element**: was
      hardcoding `llvm_type(element)` so `[Struct; N]`
      shapes panicked recursively inside the type
      builder. Switched to `llvm_type_string(element)`
      so the element can itself be an aggregate.
    - **`emit_stmt` Let-with-Array initialization**:
      hardcoded `llvm_type(element)` when emitting the
      per-element store inside an array-let. Switched
      to `llvm_type_string`.
    - **`emit_expr` Array-Index path**: same hardcode
      when GEPing into the array for `arr[i]` reads.
      Switched to `llvm_type_string`.
    Result: `[Point; 3]` arrays now compile + run on
    the LLVM backend (default), matching the C backend.
    `for c in counters { … }` over a struct array also
    works end-to-end on both backends. One new lib test
    (`for_iter_over_array_of_struct_compiles`).
    Verified end-to-end via
    `/tmp/arr_of_struct.intent` (exit 7) and
    `/tmp/for_arr_struct.intent` (exit 6 = 1+2+3).
    Vec<Struct> on LLVM remains the next gap — those
    panics fire in `vec_struct_tag` and require the
    struct-sizeof plumbing (use GEP-null trick or
    thread a struct-size map). 539 → 540 lib tests;
    47 e2e tests stay green.

36. ~~**T1.2 follow-up: [Struct;N] arrays on the C backend
    + trailing comma in array literals**~~ — done
    2026-05-20. Two related parser/codegen fixes:
    - **[Struct;N] on C backend**: the let-statement
      emit for fixed-size arrays hardcoded
      `c_leaf_type(element)` which returns the
      `/* struct */` placeholder for nominal types,
      producing invalid C declarations like
      `/* struct */ v_arr[3] = { … };`. Switched to
      `c_element_storage` which routes struct types
      through `struct_c_name` — `Struct_Point v_arr[3]
      = { … };` now compiles cleanly. End-to-end
      verified via `/tmp/arr_of_struct.intent` returning
      exit code 7 from `arr[1].x + arr[1].y` on a
      3-element Point array. LLVM backend support for
      [Struct;N] tracks the same phase-2b gap as
      Vec<Struct>.
    - **Trailing comma in array literals**: parser now
      accepts `[1, 2, 3,]` (multi-line literal with
      trailing comma on the last element) to match the
      style already accepted by struct fields, enum
      variants, and methods blocks.
    Two new lib tests
    (`array_of_struct_compiles_via_c_backend` with
    C-output `Struct_Point v_arr[3]` assertion,
    `array_literal_trailing_comma_accepted`).
    537 → 539 lib tests; 47 e2e tests stay green.

35. ~~**Checker: reserved built-in type name guard**~~ —
    done 2026-05-20. Discovered while writing a
    composite demo that `struct Task { … }` parses
    cleanly but `parse_type` promotes the identifier
    `Task` to the built-in `Type::Task` (parallel-for
    handle), so subsequent uses don't resolve to the
    user's struct. Same trap applies to `Atomic`,
    `Mutex`, `Guard`, `Channel`, `OwnedStr`, and `Self`.
    Without a gate, the user saw cryptic
    "got Task" errors deep in the pipeline. Added a
    `RESERVED_TYPE_NAMES` check in the checker's
    pre-pass: any struct/enum/type-alias decl whose name
    collides surfaces a clear
    "struct/enum/alias name 'X' is a reserved built-in
    type — pick a different name" diagnostic. Two new
    lib tests
    (`struct_name_clashing_with_builtin_rejected`,
    `enum_name_clashing_with_builtin_rejected`).
    535 → 537 lib tests; 47 e2e tests stay green.

34. ~~**T4 follow-up: Match phi-predecessor fix for nested
    sub-expressions in arm bodies**~~ — done 2026-05-20.
    The Match codegen had the same latent phi-predecessor
    bug that was fixed for IfExpr in the previous patch:
    the arm's body could introduce its own basic blocks
    (e.g., an if-expression in the arm body), but the
    phi node at the merge point used the arm's *opening*
    label as the predecessor instead of the actual tail
    BB. The codegen comment acknowledged this as "safe
    when the body doesn't introduce its own basic
    blocks". Verified the bug fires for
    `match c { Color.Red then if x > 0 { 1 } else { -1 },
    … }` — lli rejected the IR with "Instruction does
    not dominate all uses". Applied the same fix:
    snapshot `ctx.current_block` after each arm's body
    emission and use those snapshots as the phi
    predecessors. Also updates `ctx.current_block` to
    the merge label after Match. One new lib test
    (`match_arm_body_with_nested_if_expression`)
    pins the surface. Verified end-to-end via
    `/tmp/match_with_if_expr.intent` printing
    `r1= 1  r2= 10  r3= 0`. 534 → 535 lib tests; 47
    e2e tests stay green.

33. ~~**T4 if-as-expression follow-up: `else if` chaining +
    LLVM phi-predecessor tracking**~~ — done 2026-05-20.
    Two related fixes:
    - **`else if` chaining**: parser now accepts
      `if cond { … } else if cond2 { … } else { … }`
      as a single nested if-expression tree. After
      `else`, the parser dispatches: if the next token is
      `if`, recurse into `parse_primary_expr` to build a
      nested IfExpr; otherwise expect the standard `{ expr }`.
    - **LLVM phi-predecessor tracking**: nested
      if-expressions on the else side previously generated
      a `phi` node whose predecessor label was the
      branch's *opening* label, even though execution
      ended in a deeper merge block from the inner
      if-expr. lli rejected the resulting IR with "Bad
      module". Added `FnCtx.current_block` — a string
      tracking the bare name of the BB we're currently
      emitting into. Updated each time a label is
      written to `out`. The IfExpr emit now snapshots
      `current_block` after each branch's expression
      emission and uses those snapshots as the phi
      predecessors. Future Match codegen has the same
      latent issue (commented as "safe when the body
      doesn't introduce its own basic blocks") — the
      same fix can be applied there if it ever fires.
    Two new tests: `if_expression_else_if_chain_compiles`
    + `formats_if_expression_round_trip`. Verified
    end-to-end via `/tmp/if_chain.intent` which
    classifies four inputs through a 4-way chain and
    prints `a= -1  b= 0  c= 1  d= 2`. 532 → 534 lib
    tests; 47 e2e tests stay green.

32. ~~**T4 if-as-expression: `if cond { expr } else { expr }`**~~ —
    done 2026-05-20. Adds `ExprKind::IfExpr { cond,
    then_value, else_value }` (AST) and matching
    `TypedExprKind::IfExpr` (IR). Parser invokes the new
    expression form when it sees `if` at primary-expr
    position; both branches must be a single
    `{ <expr> }` (statement-bearing branches stay in
    `Stmt::If`, which `parse_stmt` reaches first when
    `if` appears at statement position). Checker
    validates `cond` is bool, type-checks both branches,
    unifies their types into the result (with a clear
    "branches have different types" diagnostic on
    mismatch). Tree-C emits a plain ternary
    `((cond) ? (then) : (else))`. Tree-LLVM emits the
    branch-then-phi shape (parallels the Match
    codegen). Formatter emits as
    `if cond { then } else { else }`. The
    branch-mutation walker, `expr_mentions` helper,
    `pin_var_to_version`, `substitute_expr`, and
    `typed_to_expr` all extended to recurse into the
    three sub-expressions. SSA path gates with a clean
    "if-expressions not yet supported" LowerError so it
    falls through to the tree backend; SMT encoder
    surfaces a clear "if-expressions not supported in
    SMT v1" Unsupported error for `prove`-side use.
    Four new lib tests (`if_expression_compiles_and_runs`
    with C-output ternary assertion,
    `if_expression_in_return`,
    `if_expression_branch_type_mismatch_rejected`,
    `if_expression_non_bool_condition_rejected`).
    End-to-end verified via `/tmp/if_expr.intent`
    returning exit code 10 from
    `if true { 10 } else { 20 }`. 528 → 532 lib tests;
    47 e2e tests stay green.

31. ~~**T1.2 follow-up: Vec<Struct> on the C backend**~~ —
    done 2026-05-20. The C backend's `element_tag`
    previously fell through to `c_leaf_type` for nominal
    types, which returns `/* struct */` for
    `Type::Struct(_)`. After the space-to-underscore
    sanitization, the resulting Vec helper name was
    `intent_vec_/*_struct_*/` — invalid C — so any
    program with `Vec<Point>` failed to compile.
    Fixed by routing `Type::Struct(name)` through
    `struct_c_name(name)` and `Type::Tuple(elements)`
    through `tuple_c_struct(elements)` in
    `element_tag`, producing
    `intent_vec_Struct_Point` etc. Vec<Struct> now
    works end-to-end on the C backend
    (`/tmp/vec_of_struct.intent` returns 3 from
    `xs[0].x + xs[0].y` on `vec(Point{1,2}, Point{3,4})`).
    One new lib test
    (`vec_of_struct_compiles_via_c_backend`) pins the
    surface + asserts the mangled name in emitted C.
    LLVM backend support for Vec<Struct> is a
    phase-2 follow-up (the `llvm_type` panic on
    struct elements + Vec helper generation for
    non-scalar elements both need work; for now users
    can use `--backend=c` on programs that need
    Vec<Struct>). 527 → 528 lib tests; 47 e2e tests
    stay green.

30. ~~**T1.3 follow-up: enum-to-integer casts (`c as i64`)
    + integer-match-pattern format round-trip**~~ —
    done 2026-05-20. Enum values can now be cast to any
    integer type via the regular `as` syntax. Useful for
    serialization, table-driven dispatch, and printing
    diagnostic values. Implementation: `explicit_cast`
    in the checker grew a fast-path arm that recognizes
    `Type::Enum(_)` source with integer destination
    and forwards through `cast_expr` (no constant folding
    yet — the variant's tag is opaque to the checker).
    LLVM `cast_opcode` treats `Type::Enum` as a 32-bit
    unsigned source (matching the i32 tag emitted by
    EnumVariant codegen) so widening uses `zext`,
    narrowing uses `trunc`. Enum→float casts stay
    rejected ("cannot cast Color to f64") — they'd need
    a less obvious `sitofp` dance and aren't a v1 use
    case. Three new lib tests
    (`enum_to_int_cast_compiles_and_runs`,
    `enum_to_smaller_int_cast_compiles`,
    `enum_to_float_cast_rejected`) pin the surface.
    Also added a `formats_integer_match_pattern_round_trip`
    test that pins the formatter's emission of
    `<int>` / `-<int>` patterns alongside variant + `_`
    arms. End-to-end verified via
    `/tmp/enum_cast.intent` — `Color.Green` becomes
    tag 1 and exits with code 1 on both backends.
    523 → 527 lib tests; 47 e2e tests stay green.

29. ~~**T1.3 follow-up: integer-literal patterns in
    match**~~ — done 2026-05-20. Match scrutinees can
    now be any integer type (i8/i16/.../u64) in addition
    to enums; the arm patterns are extended with a new
    `Pattern::Int(i128)` variant. Parser dispatches on
    the arm's first token: `_` → wildcard, `<int>` or
    `-<int>` → integer pattern (with `checked_neg`
    overflow rejection), identifier-with-`.` → variant.
    `TypedMatchArm` gained `int_value: Option<i128>` so
    backends know whether to emit `case <tag>:` (enum)
    or `case <int>:` (integer). Checker refactored to a
    unified two-mode dispatch: enum scrutinees follow the
    existing variant + exhaustiveness machinery;
    integer scrutinees require all-Int + wildcard
    arms, reject duplicate values, reject overflows of
    the scrutinee's type, and reject variant-shape
    patterns. Cross-kind diagnostics fire on mismatch
    ("integer pattern in match arm but scrutinee is of
    enum type X" / "variant pattern 'X.Y' in match arm
    but scrutinee is of integer type T"). Tree-C emits
    `case <int_value>:` instead of the variant tag;
    tree-LLVM uses the scrutinee's own LLVM type (i64
    for i64, i32 for i32, …) as the switch dispatch
    type instead of the hard-coded i32 used for enum
    tags. Five new lib tests
    (`match_on_integer_compiles_and_runs` with C-output
    `case 42:` assertion,
    `match_on_integer_with_negative_pattern`,
    `match_on_integer_requires_wildcard`,
    `match_integer_pattern_on_enum_rejected`,
    `match_variant_pattern_on_integer_rejected`).
    End-to-end verified via
    `/tmp/int_match_demo.intent` which prints
    `r1= 1000  r2= 9999  r3= -100  r4= 0`.
    518 → 523 lib tests; 47 e2e tests stay green.

28. ~~**T1.2 phase 2a follow-up #4: nested field
    assignment (`o.q.r = …;`)**~~ — done 2026-05-20.
    Refactored the LLVM FieldAssign codegen to use a
    new recursive lvalue-address helper
    (`emit_lvalue_addr`) that walks `Var(name)` →
    `FieldAccess` chains and emits a GEP chain for the
    store target. The helper handles both
    cases uniformly: a Var with owned binding uses the
    alloca slot directly; a Var with `mut ref T` uses
    the bound pointer; a FieldAccess GEPs from the
    parent's address (after dereffing the parent's type
    if it's itself a ref). The previous
    `load + insertvalue + store` path was replaced with
    `emit_lvalue_addr + GEP + store` for all FieldAssign
    cases, so single-level and nested chains share the
    same machinery. The C backend "just works" because
    chained `.` lvalues are native C — `emit_expr(o.q)`
    produces `(v_o).q` and appending `.r = 10;` yields
    valid C. The previous nested-field-assign gate in
    the checker was lifted; the previous gate-test
    (`nested_field_assign_gated_with_clean_diagnostic`)
    was replaced with `nested_field_assign_compiles_and_runs`
    which verifies a 3-level chain (Outer { q: Inner {
    r: i64 } }) compiles, emits `.q.r = 10` in C, and
    `cargo run -- run` returns the post-assign value.
    517 → 518 lib tests; 47 e2e tests stay green.

27. ~~**T1.2 phase 2a follow-up #3: methods on enums,
    method chaining, extended methods example**~~ —
    done 2026-05-20. Verified two surfaces that
    were untested before:
    - `methods on Color { fn tag(self: Color) -> i64 { … } }`
      attaches methods to enum types (the checker code
      already had the `Type::Enum(name)` arm, but lacked
      coverage; new lib test
      `methods_on_enum_basic` pins it).
    - `p.shift(5).shift(2).manhattan()` chains
      method calls (each postfix iteration of the
      parser's `.<ident>(args)` loop builds a fresh
      MethodCall whose receiver is the prior call's
      result; new lib test `method_calls_chain`).
    Extended `examples/methods.intent` to exercise
    methods on Point + Counter (mut-ref + field-assign
    chain) + Color (enum method) end-to-end with
    `assert`s suitable for the `intentc test` gate.
    Verified `cargo run -- run` prints
    `p.manhattan= 7  chained= 14  c.n= 3  color.tag= 2`.
    515 → 517 lib tests; 47 e2e tests stay green.

26. ~~**T1.2 phase 2a follow-up #2: field assignment
    (`p.x = expr;` + `self.field = expr;` through
    `mut ref`)**~~ — done 2026-05-20.
    New `Stmt::FieldAssign { object, field,
    field_span, value, span }` in the AST and matching
    `TypedStmt::FieldAssign { object, field,
    field_index, through_mut_ref, value }` in the IR.
    Parser: new `looks_like_field_assign` /
    `parse_field_assign_stmt` pair walks `<ident>(.<ident>)+
    =` to disambiguate field assignment from method calls
    (the lookahead rejects `.<ident>(` patterns), and
    builds nested `FieldAccess` for chained access like
    `outer.middle.last = value;`. Formatter emits as
    `<obj>.<field> = <value>;` and the new arm in
    `zero_stmts` zeros all sub-spans for round-trip
    parity. The checker validates that the place is
    either an owned struct or a `mut ref` to one (an
    immutable `ref` surfaces "cannot field-assign through
    an immutable ref — use mut ref T on the binding"),
    looks up the field index on the struct registry,
    rejects unknown fields with "struct 'X' has no field
    named 'y'", coerces the value type against the
    field's declared type, and threads a `through_mut_ref`
    flag into the typed stmt so backends know whether to
    deref. Tree-C codegen emits `obj.field = value;` for
    owned structs and `obj->field = value;` through a
    mut-ref pointer. Tree-LLVM codegen emits
    `getelementptr` + `store` through the alloca slot
    (insertvalue + store for owned, GEP + store directly
    for mut-ref). SSA backend gates with a clear "field
    assignment is not yet supported" LowerError so the
    SSA path falls back to the tree backend gracefully.
    Effects-checker (purity gate for `parallel for` /
    `task`) treats FieldAssign as a side-effect with a
    "cannot mutate '.field'" diagnostic. Branch-mutation
    walker, LSP walker, and SSA reads-set walker all
    extended to recurse into FieldAssign's object + value.
    Four new lib tests (`field_assign_on_owned_struct`
    asserts `.x =` appears in C output;
    `field_assign_through_mut_ref` exercises a method
    that bumps a counter;
    `field_assign_unknown_field_rejected`;
    `field_assign_through_immutable_ref_rejected`).
    End-to-end verified via
    `/tmp/field_assign_demo.intent` which prints
    `bumps: 1 2 3 final= 3`. 511 → 515 lib tests;
    47 e2e tests stay green.

25. ~~**T1.2 phase 2a follow-up: auto-ref for method
    receivers + `examples/methods.intent`**~~ —
    done 2026-05-20. The MethodCall desugar in the
    checker now inspects the resolved method's first
    parameter type: when it's `ref T` / `mut ref T`
    and the receiver is a plain value of `T`, the
    receiver expression is auto-wrapped in
    `ExprKind::Ref` / `ExprKind::RefMut` before the
    forwarded call. Users can write `p.area()` whether
    the method binds `self: Point` or `self: ref
    Point` — no manual `ref(p).area()` ceremony for
    the borrow case. Two new lib tests
    (`method_call_auto_refs_when_self_is_ref`,
    `method_call_auto_refs_when_self_is_mut_ref`)
    pin the surface. A new
    `examples/methods.intent` (value-self,
    ref-self reading consts, ref-self reading state,
    value-self returning a new instance) exercises
    the feature end-to-end with `assert` checks
    suitable for the `intentc test` pass.
    Verified `cargo run -- run` prints
    `manhattan= 7  area= 12  dist= 7  shifted_x= 13`.
    509 → 511 lib tests; 47 e2e tests stay green.

24. ~~**T1.2 phase 2a: `methods on T { … }` blocks +
    `recv.method(args)` call sugar**~~ — done 2026-05-20.
    New `methods` lexer keyword; AST
    `MethodsBlock { for_type, for_type_span, methods,
    span }` on `Program.methods_blocks`; new
    `ExprKind::MethodCall { receiver, method,
    method_span, args }`. Parser `parse_methods_block`
    wired into top-level dispatch and accepts
    `methods on TypeName { fn foo(self: TypeName, …) -> T
    { … } … }`. The postfix `.<ident>` shape now
    disambiguates: when followed by `(args)` it produces
    a `MethodCall`, otherwise (no `(`) it remains a
    `FieldAccess` so `p.x` still field-accesses cleanly.
    Formatter emits methods blocks via
    `format_methods_block` (sub-formatted function bodies
    indented by `INDENT`); method calls round-trip via a
    new `format_expr` arm. `strip_spans` extended to zero
    out method-block spans + recurse into `FieldAccess`,
    `TupleAccess`, `Tuple`, `StructLit`, `Match` so
    round-trip diffs surface only on real shape changes.
    The checker hoists each method into the regular
    function table with mangled name
    `<TypeName>_<methodName>` (after enum + alias
    resolution so the type name is accurate), drains
    `program.methods_blocks` after the hoist so
    downstream sees only ordinary functions, validates
    the methods-block target is a nominal struct/enum,
    catches duplicate methods within a block, and catches
    mangled-name collisions with existing functions or
    earlier hoisted methods. The enum-resolution +
    alias-substitution passes were extended to walk
    `methods_blocks.for_type` and each inner method's
    signature/body so resolved types reach the hoist
    pass. MethodCall expressions desugar at check time:
    the receiver is type-checked first; its
    type name yields the mangled callee; references
    through `ref T` / `mut ref T` automatically peel one
    level so `p.method()` works the same whether `p` is
    a value or a borrow; calls onto primitive/tuple/
    function-pointer receivers surface a clear
    "methods are attached to struct/enum types only" diagnostic.
    The MethodCall then forwards to the existing
    `check_call` machinery as
    `Call { name: "<T>_<method>", args: [receiver, …] }`,
    so type-checking + drop-analysis + SMT machinery work
    without further changes. Six new lib tests
    (`methods_on_struct_basic`,
    `methods_on_struct_with_extra_args`,
    `method_call_on_undeclared_method_rejected`,
    `method_call_on_primitive_rejected`,
    `methods_block_duplicate_method_rejected`,
    `methods_on_struct_field_access_via_self`) plus two
    format round-trip tests
    (`formats_methods_block_round_trip`,
    `formats_method_call_round_trip`) pin the surface.
    Verified end-to-end via
    `/tmp/method_demo.intent` which prints
    `manhattan= 7  shift= 13` when run. 501 → 509 lib
    tests; 47 e2e tests stay green.

23. ~~**T4.15 type-alias half: `type Name = Type;` top-level
    aliases**~~ — done 2026-05-20. Lexer keyword `type`;
    AST `TypeAlias { name, name_span, target, span }` on
    `Program.type_aliases`; parser `parse_type_alias`
    wired into top-level dispatch; formatter
    `format_type_alias` emits the canonical
    `type Name = Target;` shape (sorted with other
    top-level items by source position via the
    `TopItem::TypeAlias` arm); `strip_spans` zeros alias
    spans for the round-trip test. The checker
    introduces `resolve_type_aliases` (DFS with on-stack
    cycle tracking — recursive aliases surface as
    "recursive type alias 'A' is not allowed in v1"
    pointing at the offending decl) producing a
    fully-resolved `BTreeMap<String, Type>`. Alias chains
    (`type A = B; type B = i64;`) are transitively
    unfolded so the resolved map maps `A → i64`. The
    existing `resolve_enum_types_in_program` pre-pass
    was extended to also walk alias targets + const
    types so an alias pointing at an enum resolves
    `Struct(Color)` to `Enum(Color)` before alias
    substitution runs; this guarantees `type Hue =
    Color;` works regardless of source ordering. A
    second pass (`substitute_aliases_in_program`) walks
    every Type position in the program (function
    signatures, struct fields, const types,
    let/return statements in bodies) and replaces
    `Type::Struct(alias_name)` with the resolved
    target — so backends and downstream checks never
    see alias names, just concrete types. Rejects
    duplicates and collisions with struct/enum names.
    Seven new lib tests
    (`type_alias_to_primitive_compiles`,
    `type_alias_to_tuple_compiles`,
    `type_alias_to_enum_compiles`,
    `type_alias_chain_resolves`,
    `type_alias_recursive_rejected`,
    `type_alias_duplicate_rejected`,
    `type_alias_collides_with_struct_rejected`) +
    one new format round-trip pin the surface.
    493 → 501 lib tests; 47 e2e tests stay green.

22. ~~**T1.3 wildcard `_` pattern in match + new
    composite-types example**~~ — done 2026-05-20.
    AST refactored: `MatchArm.enum_name + variant` fields
    replaced by `MatchArm.pattern: Pattern` where
    `Pattern` is a new enum with `Variant { enum_name,
    variant }` and `Wildcard` arms. `TypedMatchArm` gained
    an `is_wildcard: bool` flag so backends can dispatch.
    Parser accepts a bare `_` (it lexes as the identifier
    `_` already, no new keyword needed) as a wildcard
    pattern; the wildcard arm satisfies exhaustiveness
    without listing every variant. The checker still
    enforces "every variant has an arm" *unless* a
    wildcard caught them all, and surfaces an "unreachable
    arm" diagnostic for any arm after a wildcard.
    Tree-C codegen: emit `default: __r = (body); break;`
    inside the GCC stmt-expr switch instead of
    `default: abort();` when a wildcard is present.
    Tree-LLVM codegen: route the switch's default label
    at the wildcard's basic block, skipping the
    `unreachable + abort` block entirely. Three new lib
    tests pin the surface
    (`match_wildcard_covers_remaining_variants`,
    `match_wildcard_alone_is_exhaustive`,
    `match_wildcard_followed_by_arm_rejected`). Touched
    every MatchArm use site (parser, checker, format,
    backend_c, backend_llvm, two checker
    helper functions that reverse-project typed-AST to
    surface-AST). Also added a new
    `examples/composite_types.intent` exercising
    struct + tuple + enum + match + const end-to-end;
    the SSA examples test was extended to skip examples
    that hit gated `not yet supported` lowering errors so
    new composite features don't require manual skip-list
    maintenance (the tree backend handles them; SSA path
    is gracefully recognized as out of scope). 490 → 493
    lib tests; 47 e2e tests stay green.

21. ~~**T4.15 partial: top-level `const NAME: T = literal;`
    declarations**~~ — done 2026-05-20. Lexer keyword
    `const`; AST `ConstDecl { name, name_span, ty, value,
    span }` on `Program.consts`; parser `parse_const_decl`
    wired into top-level dispatch; formatter
    `format_const_decl` emits the canonical
    `const NAME: T = expr;` shape; `strip_spans` zeros
    const spans for the round-trip test. The checker
    validates Copy-only scalar types (i64/i32/u64/.../f64/
    bool — rejects Vec/struct/tuple/string with a clear
    "v1 supports Copy scalar types only" diagnostic),
    requires the initializer to be a literal value (or
    unary-minus-of-literal so `-100` works; arithmetic +
    calls land in a later phase), rejects duplicates by
    name, and rejects collisions with declared
    structs/enums/functions. The const registry feeds into
    `check_function`, which seeds each const into the env's
    root scope as a `VarInfo` with `is_const: true` and
    `constant: Some(TypedConst::…)`. Var-resolution checks
    the `is_const` marker and substitutes the literal value
    directly into `TypedExprKind` (Int/Float/Bool) so the
    C and LLVM backends never see an unbound `v_NAME`
    reference — they get plain literal expressions and emit
    `int64_t x = 42;` rather than `int64_t x = v_NAME;`.
    Function-scoped `let NAME` cleanly shadows the const
    because the local lives in a deeper scope with
    `is_const: false`; the SMT constant-tracking pass
    continues to work unchanged because `info.constant`
    still carries the const's compile-time value. Eight
    new lib tests (`const_decl_int_compiles_and_runs`,
    `const_decl_float_and_bool`,
    `const_decl_negative_literal`,
    `const_decl_rejects_non_literal_initializer`,
    `const_decl_rejects_non_copy_type`,
    `const_decl_duplicate_rejected`,
    `const_emits_correct_value_into_c`,
    `const_can_be_shadowed_by_local`) + one new format
    round-trip pin the surface. 481 → 490 lib tests;
    47 e2e tests stay green.

20. ~~**Formatter: top-level struct/enum/interface/impl
    emission + `implement … for` parser fix**~~ —
    done 2026-05-20. Previously `format_program` emitted only
    `use`, `intent`, and `functions` — top-level
    `struct`/`enum`/`interface`/`implement` declarations were
    silently dropped from the formatted output (a real
    data-loss bug for anyone running `intentc fmt` on a
    program with composite types). Added
    `format_struct_decl`, `format_enum_decl`,
    `format_interface_decl`, `format_impl_decl`; rewired the
    top-level loop to interleave all decl categories in
    source-position order (sorted by `span.start`) so users'
    grouping (e.g. struct right above its first user) stays
    intact instead of being re-sorted into category
    buckets. Enum-variant payload syntax (`Some(T)`,
    `Err(T1, T2)`) emits in `format_enum_decl`. `format_function`
    now also emits `<T1, T2, …>` type-params and `where T is
    C, U is D, …` bound clauses so generic + bounded-generic
    signatures round-trip. Five new round-trip tests pin the
    shapes: struct decl, payload-less enum, payloaded enum,
    interface decl, impl decl. `strip_spans` extended to
    zero out struct/enum/interface/impl spans (otherwise the
    field-position drift from formatted whitespace caused
    spurious AST diffs). Two new generic round-trip tests +
    two new where-bound round-trip tests added alongside.
    Also fixed a pre-existing parser bug: `parse_impl_decl`
    used `expect_ident()` to grab the `for` keyword in
    `implement Iface for Type { … }`, but `for` lexes as
    `TokenKind::For` (it's the for-loop keyword), so any
    `implement` with a primitive-typed target (e.g. `for
    i64`) errored "expected identifier". Switched to
    `expect_keyword(…, TokenKind::For)`. 476 → 481 lib tests;
    47 e2e tests stay green.

19. ~~**T1.3 phase 2a (enum variants with payloads — parse-only
    with phase-2b WIP gate)**~~ — done 2026-05-20.
    `EnumVariant.payload: Vec<Type>` extended on the AST;
    parser accepts `Variant(T)` and `Variant(T1, T2, …)`
    after the variant name (positional types, no field
    names yet — those land in phase 2b alongside named-
    payload literals). Checker rejects any program that
    *uses* the payload syntax with a clear "T1.3 phase 2b:
    tagged-union codegen + pattern binding are still in
    progress" diagnostic — so the surface syntax compiles
    cleanly but doesn't yet produce executables. All
    existing payload-less enum code (the phase-1 surface)
    keeps working unchanged because `payload` defaults to
    empty on the legacy variants. Two new lib tests
    (`enum_variant_with_payload_parses_but_gated`,
    `enum_variant_with_multi_payload_parses_but_gated`)
    pin the gate. 470 → 472 lib tests; 47 e2e tests stay
    green.

18. ~~**T1.5 phase 1 (interfaces + `implement` + `where T is
    C` — parse-only with WIP gate)**~~ — done 2026-05-20.
    AST: `InterfaceDecl`, `InterfaceMethod`, `ImplDecl`,
    `WhereClause`; `Program.interfaces`, `Program.impls`,
    `Function.where_clauses` carry the surface declarations.
    Lexer: new keywords `Interface`, `Implement`, `Where`,
    `Is`. Parser: `parse_interface_decl`, `parse_impl_decl`,
    top-level dispatch wired; `parse_function` now also
    accepts `where T1 is C1, T2 is C2, …` clauses after the
    return type and before `requires`/`ensures`. The checker
    has a phase-1 gate: any program that declares an
    interface, an `implement` block, or a generic function
    with `where` clauses surfaces a clear "T1.5 phase 2:
    dispatch / bounded-generic checking is still in progress,
    specialize manually" diagnostic — so users who type the
    syntax learn it parses but isn't yet executable. Three
    new lib tests pin the gate
    (`interface_decl_parses_but_gated`,
    `implement_for_parses_but_gated`,
    `where_bound_parses_but_gated`). Formatter handles
    `type_params` and `where_clauses` on `Function` so
    bounded-generic signatures round-trip; interface / impl
    top-level emission is deferred to phase 2 alongside
    dispatch (no examples use them yet). 467 → 470 lib tests;
    47 e2e tests stay green. **Phase 2 still pending**:
    interface-method signature verification against impl
    methods, vtable layout + dispatch, `where T is C`
    constraint propagation through the monomorphization
    queue (depends on T1.4 phase 2), `Self` type inside
    interfaces, conflict detection on overlapping impls.

17. ~~**T0.0 syntax sweep (keyword-first refactor)**~~ —
    done 2026-05-20. Lexer added `Ref` / `From` / `To`
    keywords (`ref`, `from`, `to`). Parser rewired so type
    position accepts `ref T` / `mut ref T` (old `&T` /
    `&mut T` surfaces a "use `ref T` (T0.0)" hint), unary
    borrow accepts `ref x` / `mut ref x` (the old `&x` and
    `&mut x` prefix shape errors out with the same hint),
    for-loop range form is now `for VAR from LO to HI` (the
    `0..n` `DotDot` shape is no longer accepted at the
    parser layer though the token remains in the lexer
    table), and for-iter borrow uses `for VAR in ref XS`.
    Formatter updated: `Type::Ref` / `Type::RefMut`,
    `ExprKind::Ref` / `ExprKind::RefMut`, and both for-loop
    arms now emit the keyword shapes. `Type::Display`
    likewise. All 27 example files swept end-to-end
    (`examples/*.intent`); four lib-test source strings
    migrated (`for_loop_rejects_non_integer_bounds`,
    `ref_to_ref_rejected`, the format round-trip test, and
    `clone_at_*`); two SSA crosscheck files updated. Tests
    that pinned old diagnostics (e.g. "must be Copy",
    "Vec elements of array type") got their messages
    updated upstream when the gates were relaxed. The
    Python `r#"…"#`-aware sweeper at `/tmp/sweep.py` did
    the bulk of the test-source migration. Bitwise `&` /
    `|` / `^` remain available in binary positions
    (`reduce var with &`, `a & b`); only the PREFIX `&`
    form is gone. 456 lib + 47 e2e tests stay green.

16. ~~**Language #7 (phase 2d: non-Copy Vec slot reads via
    `clone_at(&xs, i)`)**~~ — done 2026-05-20. Bare `let inner
    = xs[i]` for non-Copy elements still errors (would alias
    the owner's slot and double-free), but the diagnostic now
    points users at the new `clone_at(&xs, i)` builtin. Added
    `"clone_at"` to `BUILTIN_FUNCTION_NAMES` + a new
    `check_clone_at_builtin` checker that accepts a `Vec<T>`
    or `&Vec<T>` source and an integer index, returning a
    fresh owned `T`. Backend emit added in:
    - **Tree-C** (`emit_call`): produces
      `intent_vec_<inner>__clone((xs).data[i])` —
      `c_element_deep_clone` routes through the inner Vec's
      `__clone` for non-Copy elements and returns the raw
      slot for Copy ones. The xs operand is wrapped in parens
      so `&xs->data[i]` parses correctly when xs is a ref.
    - **SSA-LLVM** (`emit_instr` Call arm): allocas a struct-
      pointer shadow when xs is by value, GEPs to the slot,
      and either `load`s (Copy element) or `load + call
      @intent_vec_<inner_tag>__clone` (Vec element). Helper
      name uses `vec_struct_tag(inner)` so the call points
      at the inner Vec's own clone, not the outer's.
    
    With this in, the only remaining "Vec<T> requires T:
    Copy" carve-out is the pre-existing rejection on the
    syntactic shape `let inner = xs[i]` itself — guidance
    just moved from "not yet supported" to "use clone_at".
    #7 is now fully closed end-to-end. Two new lib tests
    (`clone_at_extracts_owned_copy_of_inner_vec`,
    `clone_at_rejected_on_non_vec_collection`) pin the
    builtin's surface area. 454 → 456 lib tests; 47 e2e
    unchanged.

15. ~~**Language #7 (phase 2c: full `Vec<[T; N]>` lift)**~~ —
    done 2026-05-20. The phase-2b gate is gone — `Vec<[T;
    N]>` now compiles and runs through both C and LLVM
    backends. **C side:** new `emit_array_typedefs_for`
    pass walks each vec-element type and emits a
    per-shape typedef (`typedef int64_t
    intent_arr4_int64_t[4];`) before any helper bundle
    that references it. `emit_vec_bundle`'s `__push` /
    `__set` switch to `memcpy(xs.data[i], v, sizeof(...))`
    for array elements (C rejects `arr1 = arr2` via `=`);
    `__clone` memcpys the whole buffer since array
    elements are bytewise-Copy even though their type
    isn't `Copy` in the language sense. Vec literal call
    sites detect array elements and emit
    `(intent_arr4_int64_t[N]){ { 1,2,3,4 }, … }` (bare
    braces per slot) so gcc accepts the outer initializer
    as constant. **LLVM side:** new
    `vec_element_value_str(element)` returns the in-buffer
    value spelling — for `Array<T, N>` it's `[N x T]`,
    distinct from the SSA-value `[N x T]*` form. Without
    this the struct decl became
    `{ [N x T]**, i64, i64 }` (double-pointer) and every
    helper extract misaligned. Tree-LLVM struct-decl,
    `emit_vec_helpers`, `emit_vec_let_from_literal`, the
    `vec(...)` sub-expression path in `emit_expr`, and
    SSA-LLVM's `emit_vec_call` all route through the new
    helper. SSA-LLVM additionally loads each array
    argument into a value before storing into its buffer
    slot so the vec literal moves arrays by value. One
    lib test
    (`vec_of_fixed_array_compiles_and_runs`) pins both
    the new typedef + helper shape in the C emit. 453 →
    454 lib tests; 47 e2e unchanged.

14. ~~**Language #7 (phase 2b: `Vec<[T; N]>` clean gate)**~~ —
    done 2026-05-20. The annotation validator + vec-builtin
    check now emit a forward-pointing "not yet supported,
    wrap in `Vec<inner>`" diagnostic when the user writes
    `Vec<[T; N]>`. Full lift needs a per-shape array
    typedef (e.g. `typedef int64_t intent_arr4_int64_t[4];`)
    emitted before the helper bundle, and `__push` / `__set`
    must memcpy the slot rather than assign (C forbids
    array assignment via `=`). The LLVM side has analogous
    aggregate-store wiring left. One new lib test
    (`vec_of_fixed_array_still_rejected`) pins the
    diagnostic shape. 453 → 454 lib tests; 47 e2e unchanged.

13. ~~**Language #7 (phase 2a): `for v in &xs` over `Vec<Vec<U>>`**~~ —
    done 2026-05-20. Adds a `no_drop: bool` field to
    `VarInfo`; the for-iter checker sets it on the
    iteration variable when the form is non-consuming AND
    the element type is non-Copy.
    `emit_current_scope_drops` skips bindings with
    `no_drop == true` so the iteration view (which aliases
    `xs.data[i]`) doesn't get auto-freed and double-free at
    the outer collection's drop. Tree-LLVM's `emit_expr`
    Call arm gained a `vec(...)` sub-expression path so
    nested `vec(vec(1,2), vec(3))` literals lower
    correctly — previously the inner `vec(...)` calls
    fell through to the user-fn emit and called the
    nonexistent `@fn_vec`. All four backends (tree-C,
    SSA-C, tree-LLVM, SSA-LLVM) routed their for-iter +
    literal-emit paths through `c_element_storage` /
    `llvm_type_string` instead of `c_leaf_type` /
    `llvm_type` so nested element types resolve to their
    typedef struct names. One new lib test
    (`for_iter_borrow_over_vec_of_vec_works`) pins the
    behavior. 452 → 453 lib tests; 47 e2e unchanged.

12. ~~**Language #7 (phase 1): `Vec<T>` accepts non-Copy `T`**~~ —
    done 2026-05-20. Construction / push / drop of
    `Vec<Vec<U>>` now compiles and runs end-to-end through
    both the C and LLVM backends. Surface gate at
    `check_vec_builtin` + `validate_array_element_type`
    relaxed from "Copy" to "non-reference". Runtime
    emission rewritten with composable type tags
    (`element_tag` in C, `vec_struct_tag` in LLVM) so
    nested aggregates produce distinct identifiers;
    bundle collection recurses inner-first so a
    `Vec<Vec<i64>>` emits `intent_vec_int64_t`'s typedef +
    helpers before the outer `intent_vec_vec_int64_t`'s do.
    Element-aware paths: `__set` frees the old slot via
    inner `__free` before overwriting; `__clone` deep-
    clones each slot via inner `__clone`; new `__free`
    walks elements first then frees the outer buffer.
    All Vec-drop sites (`TypedStmt::Drop`,
    `TypedStmt::Discard`, consume-on-for-iter exit, plus
    SSA-LLVM's `InstrKind::Drop`) route through the
    per-type `__free` helper. `vec_element_byte_size`
    returns 24 for `Vec<U>` aggregates (was: 8 via the
    `element.bits()/8` fallback, causing under-allocation
    + silent heap corruption for nested Vecs). **Phase 2
    follow-ups (still pending):** indexing a non-Copy Vec
    element into a let (the checker now rejects with a
    clear "would alias and double-free" diagnostic; lift
    requires second-class refs or `take(xs, i)` builtin),
    and `Vec<[T; N]>` (fixed-size array elements stay
    Copy-only until per-slot drop hooks land). Four new
    lib tests pin: nested compile passes,
    `Vec<Vec<i64>>` indexing rejected, `Vec<&T>` still
    rejected, recursive-drop helper appears in emitted C.
    449 → 452 lib tests; 47 e2e unchanged.

11. ~~**Tooling #9: LSP scope per-block, not per-function**~~ —
    done 2026-05-20. Investigation showed the scope-aware
    identity was already wired through `binding_decl_span`
    on every `Var` read (stamped by the checker) +
    `matches_target`'s decl-span equality. The bookkeeping
    `compute_rename` docstring still warned of "no scope
    analysis"; refreshed the doc and added two lib tests
    (`compute_references_distinguishes_nested_block_shadows`,
    `compute_rename_does_not_touch_inner_block_shadow`) that
    pin the actual behavior for nest-shadowed `x` inside
    one function. 447 → 449 lib tests; 47 e2e unchanged.

10. ~~**Language #8: empty `vec()` via type-directed
    elaboration**~~ — done 2026-05-20.
    New `try_elaborate_empty_vec(expr, expected,
    diagnostics)` helper short-circuits `vec()` (no args)
    before `check_expr` errors out, when the surrounding
    context (let-annotation, reassign target type, or
    function return type) names the element. C and SSA-C
    backend emit special-cased to `intent_vec_<T>__from(0,
    NULL)` since C99 forbids zero-length array literals.
    Tree-LLVM and SSA-LLVM already handled the zero-arg
    case in their inline malloc-then-store shapes. Updated
    diagnostic for the unannotated case: "vec() needs
    either at least one element or a type annotation…".
    Two new lib tests pin both branches:
    `empty_vec_accepted_with_annotation`,
    `empty_vec_without_annotation_still_errors`. Also
    fixed a regression introduced mid-session in
    Stmt::Assign — the lookup-before-check_expr ordering
    double-freed `xs = push(xs, i)` because
    `existing.moved` was captured pre-consume; the
    permanent fix peeks at env only for the empty-vec
    recognizer and otherwise preserves the original
    check_expr-then-lookup order. 446 → 447 lib tests; 47
    e2e unchanged. Known issue entry deleted from
    STATUS.md.

9. ~~**Verifier #5 + #6: loop-preservation substitution
   precision/soundness**~~ — done 2026-05-20.
   `walk_for_reassigns` rewritten on two fronts:
   - **#5 (precision):** each `Stmt::Assign` / shadow-`Let` now
     substitutes the current map into its RHS before storing.
     A body that reassigns the same variable multiple times
     per iteration composes correctly — e.g.
     `acc = acc + 1; i = i + 1; acc = acc + 1;` produces
     `acc -> (acc + 1) + 1`, `i -> i + 1` (was: `acc -> acc + 1`,
     dropping the second composition step). Test:
     `invariant_preserves_multi_reassign_via_composition`.
   - **#6 (soundness):** nested `While`/`For`/`ForIter` bodies
     used to be ignored by the substitution walk, leaving
     stale outer entries the SMT layer folded into the entry
     assumption — an unsoundness, not just imprecision. Now
     each nested-loop body triggers a havoc pass:
     `collect_branch_mutations` enumerates the nested body's
     writes, the corresponding outer-binding type is looked
     up from `env`, and the substitution map's entry for that
     name is replaced with a fresh
     `Var(<name>__havoc_<N>)` token. New
     `prove_with_calls_extra` shim registers the fresh names
     with SMT's vars list via a new
     `verify_loop_invariants_with_havoc` entry point. Tests:
     `nested_loop_havocs_outer_var_in_substitution` (the bad
     invariant now correctly fails preservation),
     `nested_loop_no_outer_havoc_when_var_untouched`
     (untouched outer bindings still verify).
   443 → 446 lib tests; 47 e2e unchanged. Known issues entries
   for #5 and #6 deleted from STATUS.md.

## Small / contained

1. ~~**LLVM bool reductions via byte-promoted alloca**~~ — done
   2026-05-18. `emit_parallel_for_via_gomp` now allocates an i8
   shadow per bool reduction, zext-stores the current bool,
   captures the shadow address, and on exit reads back
   `icmp ne i8 …, 0` into the original i1 alloca. The atomic
   handler's `and`/`or` arm emits `atomicrmw and/or i8*` against
   the shadow with a `zext i1 → i8` on the increment. Outlined
   count bumped 7 → 8; 4 new lib tests + 1 new e2e assertion.

2. ~~**`TODO(llvm-backend)` paper-cuts**~~ — done 2026-05-18.
   17 sites in [src/backend_llvm.rs](src/backend_llvm.rs)
   addressed: 14 defensive checks converted to `unreachable!`
   with clear panic messages (so checker bugs surface loudly
   instead of emitting a silent `;` comment), and 3 reachable
   cases implemented — array let from a Var RHS now copies via
   whole-aggregate load/store (LLVM optimizes to memcpy);
   Discard of a Vec extracts the data pointer and calls `@free`
   to stop the leak; `llvm_type` now panics on
   aggregate/reference types instead of silently returning
   `i64`. 3 new lib tests, 0 regressions.

3. ~~**Memory record refresh**~~ — done 2026-05-18.
   [project_vani_backend.md](memory/project_vani_backend.md)
   (formerly `project_future_compiler_backend.md`) rewritten to reflect
   LLVM-as-default, parallelism + min/max + bool-shadow reductions, and
   the paper-cut sweep.

## Medium

4. ~~**Bitwise `&` / `|` / `^` reductions**~~ — done 2026-05-18.
   `BinaryOp::BitAnd`/`BitOr`/`BitXor` and matching `ReductionOp`
   variants landed; `Pipe` and `Caret` lexer tokens; Rust-style
   precedence (`|` < `^` < `&` < shifts) without disturbing the
   existing comparisons-above-shifts ordering. Checker uses a
   focused `check_integer_bitwise` that promotes integer types
   and rejects floats; constant-folding through
   `eval_integer_binary`. C lowering is automatic via
   `display_symbol`; LLVM emits `and`/`or`/`xor` for the binary
   form and `atomicrmw and/or/xor` at native width for the
   reduction form (no i8 shadow needed since integers are already
   byte-aligned — distinct from the bool `&&`/`||` path). SMT
   encoding via `bvand`/`bvor`/`bvxor`. 8 new lib tests + 1 lli
   end-to-end + e2e count bumps (8 → 11 outlined; 8 → 11
   pragmas).

5. ~~**`task` keyword**~~ — done 2026-05-18 (v1, sequential
   lowering). Surface: `task <name> { … }` declares an affine
   `Task` handle; `join <name>;` consumes it. Same purity
   rules as `parallel for` body (no print, no IndexAssign on
   captures, no impure calls — verified by reusing
   `verify_pure_body`). Affine tracking via env's existing
   `moved` field plus a `verify_task_affine` post-pass that
   walks the typed body and catches unjoined / double-joined
   handles even on return-terminated paths. Both backends
   lower spawn to an inline body block and `join` to a no-op
   — the verifier's race-freedom proof carries over verbatim
   to a future pthread or `@GOMP_task` lowering. 5 new lib
   tests; new `examples/tasks.intent` plumbed through the
   cross-backend parity runner.
   Done 2026-05-19: pthread-based real-threading for
   `task`. The IR's `TypedStmt::TaskSpawn` now carries
   `captures: Vec<(String, Type)>` populated during type-
   checking (a walker over the body, validated against the
   parent env). The checker enforces Copy-only captures —
   affine handles (Vec / Atomic / Mutex / Guard / Channel,
   plus arrays / OwnedStr) can't ride the pthread context
   by value, so the supported pattern is "pre-extract
   scalars before the spawn site". Both backends emit a
   per-spawn outlined function and call
   `pthread_create` / `pthread_join`. C: ctx struct +
   `static void* intent_task_<N>(void* _ctx_raw)` written to
   a `TASK_OUTLINES` thread-local and spliced between
   prototypes and function bodies; spawn site mallocs the
   ctx, populates each field with `local_name(cap)`, fires
   `pthread_create(&v_handle.thread, NULL, intent_task_<N>,
   ctx)` and stashes the ctx pointer in
   `v_handle.ctx`. LLVM: anonymous ctx struct, outlined fn
   queued onto `ctx.deferred_functions` (same
   infrastructure parallel-for outlining uses); the spawn
   site allocates the ctx via `@malloc`, loads + stores
   each captured value through a typed GEP, and calls
   `@pthread_create(i64* %thread, i8* null,
   i8* (i8*)* @intent_task_<N>, i8* %ctx)`. The join site
   on both backends loads the handle, calls pthread_join,
   then `@free`s the ctx pointer. `%intent_task_handle =
   type { i64, i8* }` (LLVM) / `typedef struct { pthread_t
   thread; void* ctx; } intent_task_handle;` (C) carry the
   same shape across backends. `examples/tasks.intent`
   updated to pre-extract scalar values (the old version
   captured the array by value, which the new Copy-only
   gate rejects). The intentc cc invocation gained the
   `-pthread` flag. 1 new lib test pins: spawn emits the
   `pthread_create` call site + outlined fn in C and the
   matching `@pthread_create` / `define internal i8*
   @intent_task_0` in LLVM.

## Concurrency primitives (after CFG/SSA)

Today's data-sharing model is "shared mutable state is
unrepresentable": read-only captures + `reduce` + affine
handles, no user-facing locks or atomics. Some patterns
(producer/consumer queues, work-stealing schedulers, lazy
shared caches) genuinely can't be expressed. These items add
escape hatches. **All three benefit from doing CFG/SSA first**
because lock-acquisition-pairing and atomic-ordering analyses
are dataflow problems that read much better off SSA.

A1. ~~**`Atomic<T>` cell**~~ — done 2026-05-18 (v1: i64 width
    only). `Type::Atomic(Box<Type>)` is recognized in type
    position; `Atomic<T>` is not Copy (affine), the
    handle is consumed at scope exit like a `Vec<T>`. Four
    sequentially-consistent builtins land:
    `atomic_new(initial)` (constructor), `atomic_load(&a)`,
    `atomic_store(&a, v)` (echoes the stored value),
    `atomic_fetch_add(&a, v)` (returns the OLD value). Checker
    requires the cell arg to be `&Atomic<T>` (either Ref
    flavor works; backends drop the `const` qualifier in C
    so `atomic_store_explicit` accepts the pointer). The C
    backend uses `<stdatomic.h>` with `memory_order_seq_cst`;
    the LLVM backend emits `load atomic … seq_cst, align 8`,
    `store atomic …`, and `atomicrmw add … seq_cst`. New
    `examples/atomics.intent` plumbed through the
    cross-backend parity runner; 3 lib tests pin positive +
    type-rule rejection.
    Done 2026-05-19: wider integer widths. `Atomic<T>` now
    accepts T ∈ {i8, i16, i32, i64, u8, u16, u32, u64}. The
    checker (`is_supported_atomic_element`) gates the element
    type at `atomic_new` and infers it from the `&Atomic<T>`
    arg for load/store/fetch_add/CAS. C backend lowers the
    storage as `_Atomic <c_leaf_type(T)>` via a new
    `c_atomic_storage` helper; load/store-stmt-expr/CAS-stmt-
    expr wrappers use `c_leaf_type(value_arg.ty)` for the
    temporary. LLVM backend gained `atomic_storage_llvm` +
    `atomic_align` and emits width-matched
    `load atomic iN, iN* …, align M` / `store atomic` /
    `atomicrmw add iN*` / `cmpxchg iN*` per element width.
    `examples/atomics.intent` extended with i32 and u8 paths
    exercised on both backends through the cross-backend
    parity runner. 3 new lib tests pin: i32 happy path, u8
    happy path, f64 rejection. `Atomic<bool>` remains
    deferred — i1 atomics require an i8-shadow with
    zext/trunc at every operand boundary; tracked as a
    follow-up.
    Done 2026-05-19: `Atomic<bool>`. The checker accepts
    `Type::Bool` in `is_supported_atomic_element` and
    rejects `atomic_fetch_add` on bool with a dedicated
    error (bool has no addition). LLVM lowering: storage
    type is i8 (atomic_storage_llvm/atomic_align extended);
    `atomic_new(b)` zexts the i1 operand to i8 before
    storing; `atomic_load` issues `icmp ne i8 …, 0` to
    truncate back to i1; `atomic_store` / CAS zext their
    bool operands to i8 before the atomic op. C lowering
    falls out naturally because `_Atomic _Bool` is a valid
    C11 type. `examples/atomics.intent` extended with a
    bool flag; both backends produce identical output. 2
    new lib tests pin: bool happy path through
    new/store/load/CAS; fetch_add rejection on bool.
    Done 2026-05-19: `&Atomic<T>` captures across
    `parallel for`. The pre-existing builtin-bypass in
    `verify_pure_body` (atomic_* names not in the
    user-signatures map, so the pure-walk treats them as
    pure-by-construction) already lets `parallel for`
    bodies call `atomic_fetch_add` / `atomic_load` /
    `atomic_store` / `atomic_compare_exchange` through a
    captured `&Atomic<T>` reference. The C and LLVM
    parallel-for outlinings already thread the captured
    pointer through their ctx structs and OpenMP shared-
    clauses. `examples/atomics.intent` extended with a
    parallel-counter section that runs over 8 iterations
    against a shared `Atomic<i64>` — both backends
    deterministically print 8. 1 new lib test pins:
    program compiles, LLVM IR contains both
    `define internal void @__intent_par_` and
    `atomicrmw add i64*` (the captured pointer survives
    the outlining), C IR contains both the
    `omp parallel for` pragma and
    `atomic_fetch_add_explicit`.
    Done 2026-05-18: `atomic_compare_exchange(&a, expected,
    new) -> bool` lands the 5th atomic builtin. C lowers via
    `atomic_compare_exchange_strong_explicit` wrapped in a
    GNU statement-expression (the success bit flows back as
    a single bool); LLVM lowers via `cmpxchg i64* …, i64 …,
    i64 … seq_cst seq_cst` with `extractvalue {i64, i1}, 1`
    pulling out the success bit. 2 lib tests + extended
    `examples/atomics.intent`.

A2. ~~**Channels**~~ — done 2026-05-18 (v1: single-slot
    rendezvous, i64 payload). `Type::Channel(Box<Type>)`,
    three builtins: `channel_new() -> Channel<T>`,
    `channel_send(&ch, v) -> T`, `channel_recv(&ch) -> T`
    (spin-waits on a ready flag). The C backend declares
    static-inline runtime helpers (`<stdatomic.h>` ops with
    `seq_cst` ordering); the LLVM backend emits inline
    `getelementptr` + `load atomic` / `store atomic` over an
    `%intent_channel_i64 = type { i64, i64 }` struct. 2 lib
    tests; cross-backend parity test runs
    `examples/concurrency.intent`.
    Done 2026-05-18: 16-slot bounded ring buffer.
    `Channel<i64>` storage is now `{ [16 x i64], head, tail
    }` (C: `int64_t buf[16]; _Atomic int64_t head; _Atomic
    int64_t tail;`). `channel_send` spins while
    `tail - head >= 16` (full), writes `buf[tail & 15]`,
    bumps tail atomically. `channel_recv` mirrors. FIFO
    order preserved across multiple `send` calls before
    `recv`. The atomicity is in place for a future MPSC
    threading lowering; for v1 sequential use the spin loops
    never fire. 1 new lib test pins multi-message
    buffering; `examples/concurrency.intent` extended with
    a send-send-send-recv-recv-recv segment that both
    backends agree on (`11, 22, 33`).
    Done 2026-05-19: multi-producer-safe tail-claim. The C
    runtime helper `intent_channel_i64_send` now CASes the
    tail to claim a unique slot before writing into the
    buffer; the LLVM `channel_send` lowering emits a
    `cmpxchg i64*`-retry loop with a fresh
    `ch_send_try` / `ch_send_write` block pair. The claim-
    before-write order means two producers seeing the same
    `t` no longer collide on the slot. A consumer that
    races a producer between CAS-claim and slot-write may
    still observe stale data; closing that window needs a
    per-slot publication counter, tracked alongside real
    threading. 2 new lib tests pin the IR shape — LLVM
    contains `cmpxchg i64*` and the new `ch_send_try`
    label; C contains
    `atomic_compare_exchange_strong_explicit(&c->tail` and
    no longer the old `atomic_store_explicit(&c->tail,
    t + 1` line.
    Done 2026-05-19: per-slot publication counter (Vyukov
    MPSC). The struct gained a sibling `seq: [16 x i64]`
    field next to `buf`. Producer protocol: load `t =
    tail`, read `seq[t & 15]`, only attempt the
    tail-CAS-claim when `seq == t` (slot is in round t).
    After winning the CAS, write `buf[t & 15] = v`, then
    publish via `store atomic seq[t & 15] = t + 1`.
    Consumer protocol: load `h = head`, wait for `seq[h &
    15] == h + 1` (producer has published), read `buf[h &
    15]`, release the slot via `store atomic seq[h & 15] =
    h + CAP`, bump head. This closes the consumer-reads-
    unpublished-slot race the CAS-tail-only design left
    open. `channel_new` initializes `seq[i] = i` so slot
    `i` starts ready for round `i`. Both backends mirror
    the shape; cross-backend parity test agrees. 2 new lib
    tests pin: C struct contains the seq array and
    publishes via the new store; LLVM struct shape +
    seq-initializer constant array literal.
    Done 2026-05-19: parametric `Channel<T, N>` + multi-width
    payloads. `Type::Channel(Box<Type>, u64)` carries the
    capacity field; the parser accepts both `Channel<T>`
    (default N=16) and `Channel<T, N>`. The checker validates
    N is a power of two ≥ 1 and T is one of the integer
    widths (`i8 .. i64`, `u8 .. u64`; bool deferred). Element
    inference at `channel_new` is solved via a channel-coerce
    arm in `coerce_checked`: `channel_new()` always returns
    the default-shaped `Channel<i64, 16>`, and the let-site
    coerce retypes the call's TypedExpr to the binding's
    declared `Channel<T, N>` (rejecting unsupported T/N at
    that point). Send/recv read (T, N) off the channel
    ref's type. Both backends now emit per-(T, N) runtime
    helpers: C uses `c_channel_storage`/`c_channel_helper`
    that build `intent_channel_<c_leaf_type>_<N>` names;
    LLVM uses `llvm_channel_struct(elt, N) ->
    %intent_channel_<llvm_type>_<N>` and emits the matching
    struct + inlined Vyukov MPSC ops per call site. A new
    pair of `pub(crate)` walkers — `collect_channel_specs`
    and the stmt/expr variants — gathers unique (T, N)
    pairs from the typed IR so the preamble emits exactly
    the bundles the program references. The
    `examples/concurrency.intent` got a new `Channel<i32,
    8>` section; both backends agree on the output (`50 60`
    after the existing six lines). 3 new lib tests pin:
    `Channel<i32, 8>` typechecks and emits per-(T,N) names
    in both backends; non-power-of-two capacity rejected;
    unsupported element types rejected.
    Done 2026-05-19: `Channel<bool>` via i8 shadow.
    `is_supported_channel_element` now accepts `Bool`. LLVM
    side: new `channel_slot_llvm(element)` returns `"i8"`
    for bool and the same as `llvm_type` for integer
    widths; `llvm_channel_struct` uses the slot's storage
    spelling so `Channel<bool, 4>` produces
    `%intent_channel_i8_4`. `channel_send` zext's i1→i8
    before storing and echoes back the original i1 at the
    language level; `channel_recv` loads i8 and emits
    `icmp ne i8 .., 0` to widen back to i1 for the caller.
    A second dedup pass on `llvm_channel_struct` keys in
    `emit_llvm` avoids the duplicate-type-definition that
    would otherwise fire when a program uses both
    `Channel<bool, N>` and `Channel<i8, N>` (same i8 backing
    struct). C side is no-op: `bool buf[N]` is byte-
    addressable natively. 1 new lib test pins:
    `Channel<bool, 4>` typechecks, C emits `intent_channel_bool_4`
    helpers, LLVM emits the i8-shadowed struct and the
    zext/icmp pattern; the prior "channel rejects bool"
    test was replaced with a "rejects unsupported element"
    test that still covers the diagnostic path via `f64`.

A3. ~~**`Mutex<T>` with RAII guards**~~ — done 2026-05-18 (v1:
    i64 payload, spin-lock). `Type::Mutex(Box<Type>)` and
    `Type::Guard(Box<Type>)`, four builtins: `mutex_new`,
    `mutex_lock(&m) -> Guard<T>`, `guard_get(&g) -> T`,
    `guard_set(&g, v) -> T`. Guards are affine; scope-exit
    drop releases the lock (the existing
    `emit_current_scope_drops` machinery hands `Drop { ty:
    Guard(_) }` to both backends, which emit the unlock
    atomic store). C backend uses
    `atomic_compare_exchange_weak_explicit` for the
    acquire-spin; LLVM emits a `cmpxchg`-retry loop on
    `%intent_mutex_i64*`. Multi-step operations under a
    single lock acquisition work — distinct from `Atomic<T>`
    where each call is one atomic op. 2 lib tests;
    cross-backend parity test runs
    `examples/concurrency.intent`.
    Done 2026-05-18: `atomic_compare_exchange` lands in A1's
    follow-up section above. Static double-acquire
    prevention also done — `VarInfo.guarded_mutex` records
    the mutex name for each `Guard<T>` binding produced by
    `mutex_lock(&Var(m))`; `check_mutex_builtin` walks env
    on every subsequent `mutex_lock(&Var(m))` and rejects
    the call if any live binding already guards `m`. The
    check is syntactic (direct `&Var` only); indirect
    references (e.g. `mutex_lock(get_ref())`) skip the
    check rather than overreport. 3 lib tests pin double-
    lock rejection, sequential lock acceptance, and
    simultaneous distinct-mutex acceptance.
    Done 2026-05-18: cross-function double-acquire
    detection. Each function's `Signature` carries a
    `locks_params: Vec<bool>` flag — one bool per parameter
    — pre-computed by walking the AST body for direct
    `mutex_lock(p)` / `mutex_lock(&p)` calls naming a
    parameter. At every call site, `check_call` scans the
    callee's locks_params; for each "locked" parameter, if
    the corresponding arg names a currently-held mutex, the
    call is rejected. Also fixed an existing hole:
    `extract_locked_mutex_name` now handles
    `mutex_lock(p)` (bare Var of reference-typed param), so
    within-function double-locks via ref parameters surface
    too. 4 new lib tests pin: cross-fn deadlock rejected,
    read-only call accepted, disjoint-mutex call accepted,
    ref-param double-lock rejected.
    Done 2026-05-18: transitive `locks_params`
    propagation. After `compute_locks_params` populates each
    function's direct-lock bits, a `propagate_locks_params`
    fixpoint pass over the call graph propagates locks_params
    via Call sites: if `f` calls `g(arg)` and `g.locks_params[i]`
    is true and `arg` names one of `f`'s parameters, then
    `f.locks_params[that_idx]` is also set. Iterates until
    stable (monotone — only sets `false → true`). 2 new lib
    tests: transitive cross-function deadlock rejected;
    transitive no-lock helper accepted.
    Done 2026-05-19: `sched_yield`-backoff lock (portable
    parking-lite). The CAS-spin acquire keeps its fast
    path (single seq_cst CAS) but, after every 32 failed
    attempts, hands the time slice back to the scheduler
    via POSIX `sched_yield()`. Contended threads stop
    burning CPU on a core where the holder may want to
    run. Both backends mirror the shape: C uses
    `<sched.h>`'s `sched_yield()`; LLVM declares the
    extern and emits a `mu_yield`/`mu_continue` block pair
    with an alloca-backed spin counter (avoids phi nodes
    across the loop). 2 new lib tests pin the C-include
    and the LLVM extern+call. True kernel-wait parking
    (futex-on-Linux) is still a follow-up; this gives a
    portable improvement that ships today.
    Done 2026-05-19: futex-based real parking on Linux.
    Both backends now drive Drepper's three-state futex
    lock — state ∈ {0=unlocked, 1=locked-no-waiters,
    2=locked-waiters-present}. C side adds runtime helpers
    `intent_mutex_futex_wait/_wake` guarded by
    `#if defined(__linux__)`; the lock helper does a fast
    CAS 0→1, on contention atomic-exchanges to 2 and parks
    in `syscall(SYS_futex, …, FUTEX_WAIT_PRIVATE, 2, …)`;
    the unlock uses `atomic_fetch_sub(state, 1)` and only
    wakes via `FUTEX_WAKE_PRIVATE` when the previous state
    was 2. Non-Linux builds retain the sched_yield
    backoff via the `#else` arm. LLVM mirrors the shape:
    `%intent_mutex_i64` storage moved to `{ i64, i32 }`
    (futex requires a 32-bit word); `declare i64
    @syscall(i64, ...)` + direct `call i64 (i64, ...)
    @syscall(i64 202, i32* …, i32 128 | 129, i32 …, ...)`
    for FUTEX_WAIT/WAKE; the lock loop uses an alloca-backed
    state variable to carry the current `c` across park→wake
    iterations without needing forward-reference phi nodes;
    the Drop emit for `Guard` follows the same fetch_sub +
    conditional-wake pattern as C. SYS_futex is hardcoded to
    202 (x86_64); other architectures need a different
    syscall number (aarch64 = 98) — out of scope for v1.
    Tests pin both sides: C helper contains
    `<linux/futex.h>` include, `intent_mutex_futex_wait/_wake`
    definitions, and the `atomic_fetch_sub` unlock; LLVM
    contains the `@syscall(i64 202, …)` calls with the
    FUTEX_WAIT(128) / FUTEX_WAKE(129) operand patterns, and
    the `{ i64, i32 }` struct layout.
    Done 2026-05-19: per-arch SYS_futex number. The LLVM
    backend's two `@syscall(i64 …, …)` call sites now read
    the syscall number from `sys_futex_for_host()`, which
    dispatches on `cfg!(target_arch)` and covers x86_64
    (202), aarch64 (98), riscv64 (98), arm (240), x86 (240),
    powerpc64 (221). Unknown hosts panic at codegen time
    with a clear "add the per-arch number" message rather
    than emitting a wrong constant that would silently
    corrupt at the kernel boundary. The C backend was
    already arch-agnostic (it uses libc's `SYS_futex`
    macro). 1 new lib test pins:
    `host_sys_futex_number()` returns Some on the host the
    suite runs on, and the known-arch sanity-checks fire
    when we're on x86_64 / aarch64.
    Done 2026-05-19: first-class function pointers
    (`fn(T1, T2, ...) -> R`). New `Type::FnPtr(params,
    ret)` carries the function signature; the parser
    recognizes the type form alongside the existing `fn`
    declaration keyword. The IR gained two variants:
    `TypedExprKind::FnRef { name, name_span }` for a bare
    identifier that resolves to a top-level function, and
    `TypedExprKind::CallIndirect { callee, args }` for a
    call whose `name` resolves to a binding of fn-ptr type
    (the checker chooses between `Call` and `CallIndirect`
    in `check_call`). Both backends lower naturally: C
    emits `R (*)(T...)` declarators via `format_declarator`
    + a fn-ptr-specific Let arm, and indirect calls fall
    out as `ptr(args)`; LLVM emits `@function_name` for
    FnRef and `call <ret> (<params>) %ptr(args)` for
    CallIndirect, with the alloca-roundtrip handled by
    treating FnPtr as a scalar in `is_scalar`. Conservative
    safety: the pure-body / parallel-for / task effects
    checker rejects any `CallIndirect` (no signature → no
    purity claim); the lock-graph passes already skip
    unknown callees, so cross-function deadlock detection
    falls back to "we know nothing about indirect calls"
    rather than making false claims. The SSA pipeline
    explicitly errors out on FnRef/CallIndirect — the
    tree-based backends handle them directly, and the
    SSA-examples test skips programs that mention a
    fn-ptr type. `examples/fn_pointers.intent` showcases:
    bare function name → fn-ptr value, passing functions
    as args, calling through a fn-ptr binding. Both
    backends print `10 14 21 81`. 3 new lib tests pin: a
    fn-ptr param + indirect call yield the matching C
    declarator and LLVM IR; arity/type mismatch on the
    fn-ptr arg fails type-checking; indirect calls inside
    a `parallel for` body are rejected by the effects
    audit.

## Large

6. **CFG/SSA IR refactor** — README Growth Path #3 calls this out.
   The IR today is tree-shaped (statements + nested expressions);
   any non-trivial dataflow pass has to reconstruct CFG/SSA on the
   fly. Refactoring to a basic-block / phi-node IR up front would
   unblock optimization passes, dead-code elim across branches, and
   better diagnostics. Plan first, then incremental — this is
   genuinely multi-session work.

   Milestones (each a separately landable session):

   6a. ~~**Type layer**~~ — done 2026-05-18.
       [src/ssa.rs](src/ssa.rs) defines `Module`, `Function`,
       `BasicBlock`, `Instruction`, `Terminator`, `Operand`,
       `Const`, `ValueId`, `BlockId` with `Display` impls.
       Uses block arguments (Cranelift / Rust MIR style)
       rather than explicit phi nodes.

   6b. ~~**Lowerer for the scalar subset**~~ — done 2026-05-18.
       `lower_function(&TypedFunction) -> Result<ssa::Function,
       LowerError>` handles Let, Reassign (non-drop), Return,
       Assert (no message), Discard, If with merge-block
       construction via block args, and the scalar
       Binary/Unary/Call/Cast/Var/Int/Bool/Float Typed-expr
       variants. Unsupported constructs return a `LowerError`
       with the variant name and span so callers can decide
       whether to skip or fail. 8 lib tests pin the block /
       terminator / block-arg shape.

   6c. ~~**Loops**~~ — done 2026-05-18. While and the
       integer-range For both lower to header / body / exit
       blocks with the header taking one block-param per
       loop-carried binding. Break / Continue become Jumps
       to the exit / header (with carry args). `modified_in_body`
       walks the typed body to pick which bindings to carry.
       3 lib tests pin the shape.

   6d. ~~**Aggregates + refs**~~ — done 2026-05-18. Added
       InstrKind variants for `StrLit`, `ArrayLit`, `Index`,
       `Len`, `RefOf`, `IndexAssign`, `Drop` plus a `ForIter`
       lowerer that desugars to a counter loop with an
       `Index` load on each iteration. The `examples/*.intent`
       parity check (`tests/ssa_examples.rs`) confirms every
       existing example successfully lowers to SSA.

   6e. ~~**Parallel constructs**~~ — done 2026-05-18. Kept
       sequential lowering (the verifier's race-freedom proof
       carries over from the existing C/LLVM backends), but
       bracketed each region with `InstrKind::Hint`
       markers: `ParallelForBegin { reductions }` /
       `ParallelForEnd`, and `TaskBegin` / `TaskEnd` /
       `TaskJoin`. Reduction metadata is recorded on the
       begin-hint so a backend or analysis can recognize the
       shape without re-walking the surrounding code.

   6f. ~~**Two representative SSA-based analysis passes**~~ —
       done 2026-05-18. Both live in
       [src/ssa_pass.rs](src/ssa_pass.rs).
       - [`fold_constants`] — single-pass constant folding:
         substitutes constant operands and folds purely
         constant binary/unary/cast instructions with
         overflow-aware `checked_*` arithmetic, bool
         short-circuiting, and integer comparisons.
       - [`dce_module`] — branch threading +
         unreachable-block removal + dead-instruction
         elimination, run to fixed point. Composes with
         constant folding: folded `Const(Bool(true))` branch
         conditions thread to a `Jump`, the now-dead arm
         becomes unreachable, and any pure-no-trap
         instructions whose result is unused are dropped.
         Conservative on traps: leaves `Div`/`Rem`/`Shl`/
         `Shr` binaries, indexing, calls, allocations, and
         drops alone. Returns a `DceStats` struct so tests
         and tools can observe each sub-pass's bite.
       10 lib tests pin the rewriting and composition
       behavior.
       **Migration progress** (originally three follow-ups;
       SSA-bounds-elision landed below):
       Done 2026-05-19: SSA-based bounds-elision starter.
       New `ssa_pass::elide_bounds(module)` walks every
       `InstrKind::Index` / `IndexAssign` and flips the
       `checked` flag when the index operand is a constant
       in-bounds against a statically-known array extent
       (ArrayLit, parameter `[T; N]`, or `RefOf` to one). A
       per-function `BTreeMap<ValueId, u64>` records known
       lengths via `collect_array_lengths`. Returns
       `ElideStats { indexed_loads_elided, index_stores_elided }`
       for observability. Today this complements the
       typed-IR SMT pass; once codegen migrates to SSA
       (#6g) the typed pass goes away. 3 new lib tests pin
       constant-in-bounds elision, variable-index pass-
       through (no elision), and IndexAssign elision via
       base type's `Array { length }` field.
       Done 2026-05-19: SSA drop-coverage audit. New
       `ssa_pass::audit_drops(module) -> DropAudit` walks
       every function and counts (a) affine value
       constructions (every instruction whose result type is
       Vec/OwnedStr/Atomic/Channel/Mutex/Guard/Task), (b)
       `InstrKind::Drop` instructions emitted, and (c)
       discrepancies between the two. The typed-IR
       drop-insertion still owns INSERTION; this audit
       provides the SSA-side observability layer so a future
       SSA-driven inserter has a verifier from day one. 2
       new lib tests pin: Vec round-trip produces ≥1 Drop;
       Atomic + Mutex + Guard each get their drops.
       Done 2026-05-19: SSA-side effects audit. New
       `ssa_pass::audit_pure_regions(module) ->
       Vec<PureViolation>` tracks a depth counter through
       `Hint::ParallelForBegin/End` and `Hint::TaskBegin/End`
       markers and flags impure instructions inside regions:
       `IndexAssign`, `Drop` of non-Copy types, and calls
       into the heap-allocating Vec builtins (`vec`,
       `push`, `set`, `clone`). Per-violation records carry
       the function name, violation kind (`IndexAssign{array}`,
       `DropNonCopy{name, ty}`, `VecBuiltinCall{name}`), and
       source span. The typed-IR effects checker remains the
       authoritative gate; this audit defends the SSA-side
       invariant against future rewrites that might
       inadvertently introduce impure ops into a marked
       region. 4 new lib tests pin: clean parallel-for /
       task / non-region programs pass; a synthetic
       hand-injected `IndexAssign` inside a parallel-for
       region surfaces as the expected violation kind.
       Done 2026-05-18: reduction-shape recognizer
       [`recognize_reduction_shapes`] walks each function
       and returns one `ReductionShape { function,
       begin_block, end_block, reductions }` per matched
       `Hint::ParallelForBegin` / `ParallelForEnd` pair.
       Unbalanced markers surface in a parallel `unmatched`
       list rather than panicking. A future SSA-based
       parallel-for backend lowering can consume this
       analysis instead of re-walking blocks. 4 lib tests.

   6g. ~~**SSA-consuming C backend (proof-of-concept)**~~ —
       done 2026-05-18 for the scalar subset (integers, bools,
       arithmetic, comparisons, casts, if/else, while, calls,
       returns). Lives in
       [src/ssa_backend_c.rs](src/ssa_backend_c.rs); emits a
       function per SSA `Function` with block params as
       declared C locals and terminators as goto-labels.
       Cross-check test
       [tests/ssa_backend_c_crosscheck.rs](tests/ssa_backend_c_crosscheck.rs)
       runs six curated programs through SSA → C → cc and
       asserts the exit codes match expectations.
       **Remaining migration work** (each a separately
       landable session):
       - extend SSA-C to cover aggregates (Vec/Array/Str/refs),
         parallel-for + reductions (use libgomp), tasks;
       - mirror the SSA-C work in
         [src/backend_llvm.rs](src/backend_llvm.rs) so LLVM IR
         is emitted from `ssa::Module`;
       - switch `intentc emit` / `intentc run` / `intentc
         build` to the SSA path once both backends cover the
         full feature surface (currently the tree-based
         backends remain authoritative).

7. ~~**LSP**~~ — done 2026-05-18 (v1, minimal surface).
   New `intent-lsp` binary in [src/bin/lsp.rs](src/bin/lsp.rs)
   speaks LSP over stdio via `lsp-server` + `lsp-types`. Logic
   lives in [src/lsp.rs](src/lsp.rs):
   - `textDocument/didOpen` / `didChange` / `didClose` maintain
     an in-memory document map; closes clear stale diagnostics.
   - `textDocument/publishDiagnostics` pushes the
     lexer/parser/checker errors on every open and change,
     converting byte spans to UTF-16 line/character ranges.
   - `textDocument/hover` walks the typed IR for the smallest
     `TypedExpr` covering the cursor and returns its inferred
     type. Returns nothing while the document doesn't compile.
   Drops the `Connection` before joining io threads so the
   writer mpsc closes and the server exits cleanly on
   shutdown/exit. 6 new lib tests + an end-to-end Python smoke
   test against the binary.
   Done 2026-05-18: goto-definition. `textDocument/definition`
   handler walks the typed IR at the cursor for a `Var` /
   `Ref` / `RefMut` binding name, then scans the surrounding
   function for the matching `Let` / `For` var-binding / Task
   handle declaration. Returns a `Location` whose `Range` is
   the let's RHS expression span (the closest available
   stand-in for the declaration's own span, since
   `TypedStmt::Let` doesn't yet carry one). Synthetic names
   (`__intent_ret_*`, `__intent_iter_idx_*`, …) are filtered
   so the editor only lands on user-written declarations. 3
   new lib tests pin: let-binding match, non-name cursor
   (None), broken-document fallback (None).
   Done 2026-05-18: find-references. `textDocument/references`
   handler shares the binding-resolution logic with
   goto-definition; walks every function body once to
   collect every `Var` / `Ref` / `RefMut` whose name
   matches. Honors `includeDeclaration`. Same synthetic-name
   filtering and same shadowing limitation (no scope
   analysis yet). 3 new lib tests pin: multi-use collection,
   include-declaration toggle, no-name cursor returns None.
   Done 2026-05-18: rename. `textDocument/rename` handler
   reuses `compute_references` to collect every occurrence,
   prepends the declaration span, and returns a
   `WorkspaceEdit` (via `DocumentChanges::Edits`) whose
   `TextEdit`s rewrite each span with the new name.
   Validates the new name client-side: must match
   `[A-Za-z_][A-Za-z0-9_]*` and must not collide with a
   reserved keyword (full list mirrors the lexer's keyword
   dispatch). Invalid names return an `Err(message)` so the
   editor's rename UI surfaces the failure. No-op rename
   (new name == old) returns `Some(vec![])` — empty edits.
   5 new lib tests pin: happy path multi-span, invalid
   identifier, keyword collision, broken-document None,
   no-op empty.
   Done 2026-05-18: completion.
   `textDocument/completion` handler with no trigger
   characters (Ctrl+Space invocation). `compute_completion`
   always emits language keywords + type names + the fixed
   builtin function set, so the editor's popup is useful
   even when the document doesn't compile. When the document
   compiles, every top-level function name and every
   in-scope binding (parameters + Let / For var / ForIter
   var / TaskSpawn handle declared before the cursor) is
   added. Walks every function (no scope refinement yet),
   filters synthetic names, dedups by label. 6 new lib
   tests pin: broken-doc keyword + builtin emission;
   bindings before cursor included; bindings after cursor
   excluded; function parameters present; other function
   names callable; synthetic-name filtering.
   Done 2026-05-18: code actions + semantic tokens.
   - `textDocument/codeAction` handler with
     `CodeActionKind::QUICKFIX`. Inspects every diagnostic
     in `params.context.diagnostics`, recognizes
     `expected '<TOK>'` messages where `<TOK>` is a single
     character, and emits an "Insert `<TOK>`" quick fix
     whose `WorkspaceEdit` inserts that token at the
     diagnostic's `range.end`. Marked `is_preferred: true`
     so auto-on-save fixers pick it up. 3 lib tests.
   - `textDocument/semanticTokens/full` handler with a
     6-entry legend (`variable`, `function`, `type`,
     `keyword`, `number`, `string`). Re-lexes the source
     and assigns each token a type by `TokenKind`: type
     primitives + `Vec` → `type`; `min`/`max` → `function`;
     `Int`/`Float` → `number`; `Str` → `string`; known
     type-position idents (`Atomic`, `Channel`, `Mutex`,
     `Guard`, `Task`, `Str`, `OwnedStr`, `Vec`) → `type`;
     remaining idents → `variable`; rest of the named
     keywords → `keyword`. Empty on lex error.
     Delta-format encoded per LSP spec. 5 lib tests.
   Done 2026-05-18: scope-aware references / rename /
   completion. `TypedExpr` gained a `binding_decl_span:
   Option<Span>` populated at Var / Ref / RefMut
   construction sites in the checker (via env lookup of
   `VarInfo.decl_span`). The LSP walkers'
   `find_var_at`/`collect_var_uses`/`find_declaration_span`
   now use a `Target { name, decl_span }` struct so two
   same-name bindings in different scopes have distinct
   identities. `TypedFunction` also gained a `span` field
   so `compute_completion` can identify "which function
   does the cursor belong to" and stop leaking sibling-
   function parameters. Two new lib tests pin the fixes:
   references no longer cross function boundaries on
   same-name conflict; completion no longer surfaces a
   sibling function's params. The README's "shadowing
   caveat" notes are gone.
   Done 2026-05-19: semantic-token modifiers. Legend now
   advertises `declaration` and `readonly` modifier names.
   The override map's value type became
   `TokenOverride { token_type, modifiers }`. Param
   declarations get `(parameter, declaration|readonly)`;
   `Var` reads whose `binding_decl_span` resolves to a
   parameter get `(parameter, readonly)` so editors render
   parameter uses with the parameter tint and a readonly
   underscore-style decoration. The lex emit loop writes
   `token_modifiers_bitset` from the override (still 0 for
   non-IR-resolved tokens). 2 new lib tests pin: param decl
   carries `declaration|readonly`; param read carries
   `readonly` only and the `parameter` tint.
   Done 2026-05-18: IR-driven semantic-tokens refinement.
   `ExprKind::Call` and `Param` (AST + IR) gained a
   `name_span: Span` field threaded through the parser and
   checker. `compute_semantic_tokens` now compiles the
   source alongside the re-lex; when it succeeds the typed
   IR is walked to build a `HashMap<Span, u32>` of overrides
   (`Call.name_span → function`, `Param.name_span →
   parameter`). The lex-token loop consults the override
   map and emits the refined token type when there's a hit,
   otherwise falls back to the keyword/type/variable
   heuristic. Legend extended from 6 to 7 entries to add
   `parameter`. Two new lib tests pin the refinement:
   callee identifier of a `Call` carries `function`;
   declaration-site of a function parameter carries
   `parameter`. The existing `variable`-default test still
   holds because the function-declaration name (not a
   `Call`) and let-binding name (not a `Param`) remain
   variable. `declaration` / `readonly` modifiers are
   future work.

## Deferred — after the language gains more features

Today the LLVM and C backends are sufficient to develop the
language surface. Adding more backends would split testing effort
and slow language work. Revisit once the language is closer to
its target shape.

8. **Cranelift backend** — would give a fast JIT path independent
   of LLVM. Easier after the CFG/SSA refactor (#6) lands.

9. **Direct-asm targets** — teaching path and tiny-target option
   (x86_64-linux first). Smaller surface than LLVM/Cranelift but
   tedious; not on the critical path. Easier after #6.
