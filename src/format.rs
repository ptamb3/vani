//! Source pretty-printer.
//!
//! Walks a parsed `Program` and emits canonical .intent source. Used
//! by the `intentc fmt` subcommand. Two entry points:
//!
//!  - [`format_program`] for a stripped-of-comments rendering. Used
//!    by tests that compare AST shape across a round-trip.
//!  - [`format_program_with_comments`] for the user-facing path. The
//!    caller pairs the parsed `Program` with a comment list obtained
//!    from [`crate::lexer::extract_comments`], and this fn
//!    interleaves them at the correct indent level so `// …` lines
//!    survive `intentc fmt`.
//!
//! Acceptance criteria:
//!  - `parse(format(parse(src))) == parse(src)` for every well-formed
//!    program. Whitespace and comment placement may differ; the AST
//!    must not. The round-trip is exercised over all examples in
//!    `fmt_roundtrips_every_example`.
//!  - For a source with leading or interstitial `// …` comments,
//!    `format_program_with_comments` retains every comment somewhere
//!    in the output, in document order, at the indent of the
//!    surrounding context.

use crate::ast::{Expr, ExprKind, Function, Param, PrintItem, Program, Stmt, UnaryOp};
use crate::lexer::Comment;

const INDENT: &str = "  ";

/// Carries the side-channel comment list, an advancing cursor, and
/// the original source so the recursive emit functions can:
///   - drain comments whose byte span precedes the next AST node;
///   - decide whether a comment had a blank line before it in
///     source (so we can preserve the group separator);
///   - decide whether a comment is on the same source line as the
///     statement that just ended (a trailing comment).
struct FmtCtx<'a> {
    source: &'a str,
    comments: &'a [Comment],
    cursor: usize,
}

impl<'a> FmtCtx<'a> {
    fn new(source: &'a str, comments: &'a [Comment]) -> Self {
        Self { source, comments, cursor: 0 }
    }

    /// True iff at least two newlines (possibly with intervening
    /// whitespace) precede `pos` before hitting any non-whitespace
    /// byte. Used to detect a source-level blank line just before a
    /// comment, so the formatter can preserve the visual group
    /// separation.
    fn has_blank_line_before(&self, pos: usize) -> bool {
        let bytes = self.source.as_bytes();
        // Tests that call `format_program` (no source) pass an empty
        // string; AST spans then point past the end. In that case
        // there's no source-level whitespace to mirror.
        if pos > bytes.len() {
            return false;
        }
        let mut newlines = 0;
        let mut i = pos;
        while i > 0 {
            i -= 1;
            match bytes[i] {
                b'\n' => {
                    newlines += 1;
                    if newlines >= 2 {
                        return true;
                    }
                }
                b' ' | b'\t' | b'\r' => continue,
                _ => return false,
            }
        }
        false
    }

    /// True iff `pos` and the next pending comment lie on the same
    /// source line (i.e., the bytes between contain no `\n`). Used
    /// to identify a same-line trailing comment.
    fn next_comment_same_line_as(&self, pos: usize) -> bool {
        if self.cursor >= self.comments.len() {
            return false;
        }
        let start = self.comments[self.cursor].span.start;
        if start < pos {
            return false;
        }
        let bytes = self.source.as_bytes();
        if start > bytes.len() {
            return false;
        }
        let between = &bytes[pos..start];
        !between.contains(&b'\n')
    }

    /// Emit every comment whose start byte precedes `pos`, each on
    /// its own line at the given indent. Trailing whitespace on the
    /// comment line is trimmed; the leading `//` is preserved as-is.
    /// A source-level blank line before a comment is preserved
    /// unless that would create a leading blank in the output or a
    /// blank right after an opening `{`.
    fn drain_to(&mut self, pos: usize, indent: usize, out: &mut String) {
        let pad = INDENT.repeat(indent);
        while self.cursor < self.comments.len()
            && self.comments[self.cursor].span.start < pos
        {
            let cspan_start = self.comments[self.cursor].span.start;
            if self.has_blank_line_before(cspan_start)
                && !out.is_empty()
                && out.ends_with('\n')
                && !out.ends_with("\n\n")
                && !out.trim_end_matches('\n').ends_with('{')
            {
                out.push('\n');
            }
            out.push_str(&pad);
            out.push_str(self.comments[self.cursor].text.trim_end());
            out.push('\n');
            self.cursor += 1;
        }
    }

    /// If the next pending comment sits on the same source line as
    /// `pos`, attach it as a trailing comment on the last emitted
    /// line in `out`. `pos` should be the end-byte of the AST node
    /// just emitted (e.g. just past its `;`).
    fn try_attach_trailing(&mut self, pos: usize, out: &mut String) {
        if !self.next_comment_same_line_as(pos) {
            return;
        }
        let c = &self.comments[self.cursor];
        if out.ends_with('\n') {
            out.pop();
        }
        out.push_str("  ");
        out.push_str(c.text.trim_end());
        out.push('\n');
        self.cursor += 1;
    }

    /// If the source had a blank line just before `pos`, mirror
    /// that into the output (unless the output is empty or already
    /// ends with a blank, or sits right after an opening `{`).
    /// Called immediately before emitting an AST node that may have
    /// had a source-level blank line in front of it.
    fn maybe_preserve_blank(&self, pos: usize, out: &mut String) {
        if self.has_blank_line_before(pos)
            && !out.is_empty()
            && out.ends_with('\n')
            && !out.ends_with("\n\n")
            && !out.trim_end_matches('\n').ends_with('{')
        {
            out.push('\n');
        }
    }

    /// Drain any comments left over after every AST node has been
    /// emitted (i.e. trailing comments at end of file). Always at
    /// column 0.
    fn drain_remaining(&mut self, out: &mut String) {
        while self.cursor < self.comments.len() {
            let cspan_start = self.comments[self.cursor].span.start;
            if self.has_blank_line_before(cspan_start)
                && !out.is_empty()
                && out.ends_with('\n')
                && !out.ends_with("\n\n")
            {
                out.push('\n');
            }
            out.push_str(self.comments[self.cursor].text.trim_end());
            out.push('\n');
            self.cursor += 1;
        }
    }
}

/// Canonical pretty-print, comments dropped. Convenient for tests
/// and tools that don't care about comment preservation.
pub fn format_program(p: &Program) -> String {
    format_program_with_comments(p, "", &[])
}

/// Pretty-print with comments interleaved at their original
/// positions. Preserves both blank lines between comment groups and
/// same-line trailing comments after statements. Trailing comments
/// after top-level items are also attached on the same line.
pub fn format_program_with_comments(
    p: &Program,
    source: &str,
    comments: &[Comment],
) -> String {
    let mut out = String::new();
    let mut ctx = FmtCtx::new(source, comments);
    for u in &p.uses {
        ctx.drain_to(u.span.start, 0, &mut out);
        ctx.maybe_preserve_blank(u.span.start, &mut out);
        out.push_str(&format!("use \"{}\";\n", escape_string(&u.path)));
        ctx.try_attach_trailing(u.span.end, &mut out);
    }
    if !p.uses.is_empty() {
        out.push('\n');
    }
    for intent in &p.intents {
        ctx.drain_to(intent.span.start, 0, &mut out);
        ctx.maybe_preserve_blank(intent.span.start, &mut out);
        out.push_str(&format!("intent \"{}\";\n", escape_string(&intent.text)));
        ctx.try_attach_trailing(intent.span.end, &mut out);
    }
    if !p.intents.is_empty() {
        out.push('\n');
    }
    // Top-level items beyond uses/intents: structs, enums,
    // interfaces, impls, and functions. Emit in source order
    // by span.start so the formatter preserves the user's
    // grouping (struct decls near their usage, etc.) instead
    // of imposing a category-then-order sort. Indices come
    // back tagged so we can dispatch to the right printer.
    enum TopItem {
        Struct(usize),
        Enum(usize),
        Interface(usize),
        Impl(usize),
        Const(usize),
        TypeAlias(usize),
        Methods(usize),
        Function(usize),
    }
    let mut order: Vec<(usize, TopItem)> = Vec::new();
    for (i, s) in p.structs.iter().enumerate() {
        order.push((s.span.start, TopItem::Struct(i)));
    }
    for (i, e) in p.enums.iter().enumerate() {
        order.push((e.span.start, TopItem::Enum(i)));
    }
    for (i, ifc) in p.interfaces.iter().enumerate() {
        order.push((ifc.span.start, TopItem::Interface(i)));
    }
    for (i, im) in p.impls.iter().enumerate() {
        order.push((im.span.start, TopItem::Impl(i)));
    }
    for (i, c) in p.consts.iter().enumerate() {
        order.push((c.span.start, TopItem::Const(i)));
    }
    for (i, a) in p.type_aliases.iter().enumerate() {
        order.push((a.span.start, TopItem::TypeAlias(i)));
    }
    for (i, m) in p.methods_blocks.iter().enumerate() {
        order.push((m.span.start, TopItem::Methods(i)));
    }
    for (i, f) in p.functions.iter().enumerate() {
        order.push((f.span.start, TopItem::Function(i)));
    }
    order.sort_by_key(|(pos, _)| *pos);
    for (idx, (_, item)) in order.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        match item {
            TopItem::Struct(i) => {
                let s = &p.structs[*i];
                ctx.drain_to(s.span.start, 0, &mut out);
                ctx.maybe_preserve_blank(s.span.start, &mut out);
                format_struct_decl(s, &mut out);
            }
            TopItem::Enum(i) => {
                let e = &p.enums[*i];
                ctx.drain_to(e.span.start, 0, &mut out);
                ctx.maybe_preserve_blank(e.span.start, &mut out);
                format_enum_decl(e, &mut out);
            }
            TopItem::Interface(i) => {
                let ifc = &p.interfaces[*i];
                ctx.drain_to(ifc.span.start, 0, &mut out);
                ctx.maybe_preserve_blank(ifc.span.start, &mut out);
                format_interface_decl(ifc, &mut out);
            }
            TopItem::Impl(i) => {
                let im = &p.impls[*i];
                ctx.drain_to(im.span.start, 0, &mut out);
                ctx.maybe_preserve_blank(im.span.start, &mut out);
                format_impl_decl(im, &mut ctx, &mut out);
            }
            TopItem::Const(i) => {
                let c = &p.consts[*i];
                ctx.drain_to(c.span.start, 0, &mut out);
                ctx.maybe_preserve_blank(c.span.start, &mut out);
                format_const_decl(c, &mut out);
            }
            TopItem::TypeAlias(i) => {
                let a = &p.type_aliases[*i];
                ctx.drain_to(a.span.start, 0, &mut out);
                ctx.maybe_preserve_blank(a.span.start, &mut out);
                format_type_alias(a, &mut out);
            }
            TopItem::Methods(i) => {
                let m = &p.methods_blocks[*i];
                ctx.drain_to(m.span.start, 0, &mut out);
                ctx.maybe_preserve_blank(m.span.start, &mut out);
                format_methods_block(m, &mut ctx, &mut out);
            }
            TopItem::Function(i) => {
                let f = &p.functions[*i];
                ctx.drain_to(f.span.start, 0, &mut out);
                ctx.maybe_preserve_blank(f.span.start, &mut out);
                format_function(f, &mut ctx, &mut out);
            }
        }
    }
    ctx.drain_remaining(&mut out);
    out
}

fn format_const_decl(c: &crate::ast::ConstDecl, out: &mut String) {
    out.push_str("const ");
    out.push_str(&c.name);
    out.push_str(": ");
    out.push_str(&format!("{}", c.ty));
    out.push_str(" = ");
    format_expr(&c.value, false, out);
    out.push_str(";\n");
}

fn format_type_alias(a: &crate::ast::TypeAlias, out: &mut String) {
    out.push_str("type ");
    out.push_str(&a.name);
    out.push_str(" = ");
    out.push_str(&format!("{}", a.target));
    out.push_str(";\n");
}

fn format_methods_block(
    m: &crate::ast::MethodsBlock,
    ctx: &mut FmtCtx,
    out: &mut String,
) {
    out.push_str("methods on ");
    out.push_str(&format!("{}", m.for_type));
    out.push_str(" {\n");
    for (i, method) in m.methods.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        // Reuse `format_function` into a sub-buffer, then
        // indent each line so the methods read like
        // members of the block.
        let mut sub = String::new();
        format_function(method, ctx, &mut sub);
        for line in sub.lines() {
            if line.is_empty() {
                out.push('\n');
            } else {
                out.push_str(INDENT);
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out.push_str("}\n");
}

fn format_struct_decl(s: &crate::ast::StructDecl, out: &mut String) {
    out.push_str("struct ");
    out.push_str(&s.name);
    out.push_str(" {\n");
    for f in &s.fields {
        out.push_str(INDENT);
        out.push_str(&f.name);
        out.push_str(": ");
        out.push_str(&format!("{}", f.ty));
        out.push_str(",\n");
    }
    out.push_str("}\n");
}

fn format_enum_decl(e: &crate::ast::EnumDecl, out: &mut String) {
    out.push_str("enum ");
    out.push_str(&e.name);
    out.push_str(" {\n");
    for v in &e.variants {
        out.push_str(INDENT);
        out.push_str(&v.name);
        if !v.payload.is_empty() {
            out.push('(');
            for (i, ty) in v.payload.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&format!("{}", ty));
            }
            out.push(')');
        }
        out.push_str(",\n");
    }
    out.push_str("}\n");
}

fn format_interface_decl(ifc: &crate::ast::InterfaceDecl, out: &mut String) {
    out.push_str("interface ");
    out.push_str(&ifc.name);
    out.push_str(" {\n");
    for m in &ifc.methods {
        out.push_str(INDENT);
        out.push_str("fn ");
        out.push_str(&m.name);
        out.push('(');
        for (i, p) in m.params.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            format_param(p, out);
        }
        out.push(')');
        out.push_str(" -> ");
        out.push_str(&format!("{}", m.return_type));
        out.push_str(";\n");
    }
    out.push_str("}\n");
}

fn format_impl_decl(im: &crate::ast::ImplDecl, ctx: &mut FmtCtx, out: &mut String) {
    out.push_str("implement ");
    out.push_str(&im.interface_name);
    out.push_str(" for ");
    out.push_str(&format!("{}", im.for_type));
    out.push_str(" {\n");
    for (i, m) in im.methods.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        // Indent the method body by emitting via format_function
        // into a sub-buffer, then indenting each line by INDENT.
        let mut sub = String::new();
        format_function(m, ctx, &mut sub);
        for line in sub.lines() {
            if line.is_empty() {
                out.push('\n');
            } else {
                out.push_str(INDENT);
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out.push_str("}\n");
}

fn format_function(f: &Function, ctx: &mut FmtCtx, out: &mut String) {
    if f.is_pure {
        out.push_str("pure ");
    }
    out.push_str("fn ");
    out.push_str(&f.name);
    if !f.type_params.is_empty() {
        out.push('<');
        for (i, tp) in f.type_params.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(tp);
        }
        out.push('>');
    }
    out.push('(');
    for (i, p) in f.params.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        format_param(p, out);
    }
    out.push(')');
    out.push_str(" -> ");
    out.push_str(&format!("{}", f.return_type));
    if !f.where_clauses.is_empty() {
        out.push_str(" where ");
        for (i, w) in f.where_clauses.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&w.type_param);
            out.push_str(" is ");
            out.push_str(&w.interface_name);
        }
    }
    let has_contracts = !f.requires.is_empty() || !f.ensures.is_empty();
    if has_contracts {
        out.push('\n');
        for e in &f.requires {
            out.push_str("requires ");
            format_expr(e, false, out);
            out.push_str(";\n");
        }
        for e in &f.ensures {
            out.push_str("ensures ");
            format_expr(e, false, out);
            out.push_str(";\n");
        }
        out.push_str("{\n");
    } else {
        // No contracts — brace on the same line as the signature,
        // matching the existing `examples/*.intent` style.
        out.push_str(" {\n");
    }
    for s in &f.body {
        format_stmt(s, 1, ctx, out);
    }
    // Comments that appeared inside the body after the last
    // statement (e.g. `// fallthrough` just before `}`) live in the
    // range (last_stmt.span.end, function.span.end). Drain at the
    // body's indent before closing.
    ctx.drain_to(f.span.end, 1, out);
    out.push_str("}\n");
}

fn format_param(p: &Param, out: &mut String) {
    out.push_str(&p.name);
    out.push_str(": ");
    out.push_str(&format!("{}", p.ty));
}

fn format_stmt(s: &Stmt, depth: usize, ctx: &mut FmtCtx, out: &mut String) {
    let pad = INDENT.repeat(depth);
    // Drain any comments that appeared before this statement,
    // emitting them at the same indent so they read as leading
    // comments for the statement. Then mirror any source-level
    // blank line just above the statement, so logical groupings
    // are preserved.
    ctx.drain_to(s.span().start, depth, out);
    ctx.maybe_preserve_blank(s.span().start, out);
    match s {
        Stmt::Let { name, annotation, expr, .. } => {
            out.push_str(&pad);
            out.push_str("let ");
            out.push_str(name);
            if let Some(ty) = annotation {
                out.push_str(": ");
                out.push_str(&format!("{}", ty));
            }
            out.push_str(" = ");
            format_expr(expr, false, out);
            out.push_str(";\n");
        }
        Stmt::LetTuple { names, annotation, expr, .. } => {
            out.push_str(&pad);
            out.push_str("let (");
            for (i, n) in names.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(n);
            }
            out.push(')');
            if let Some(ty) = annotation {
                out.push_str(": ");
                out.push_str(&format!("{}", ty));
            }
            out.push_str(" = ");
            format_expr(expr, false, out);
            out.push_str(";\n");
        }
        Stmt::Return { expr, .. } => {
            out.push_str(&pad);
            out.push_str("return ");
            format_expr(expr, false, out);
            out.push_str(";\n");
        }
        Stmt::Assert { expr, message, .. } => {
            out.push_str(&pad);
            out.push_str("assert ");
            format_expr(expr, false, out);
            if let Some(m) = message {
                out.push_str(", \"");
                out.push_str(&escape_string(m));
                out.push('"');
            }
            out.push_str(";\n");
        }
        Stmt::Prove { expr, .. } => {
            out.push_str(&pad);
            out.push_str("prove ");
            format_expr(expr, false, out);
            out.push_str(";\n");
        }
        Stmt::Print { items, .. } => {
            out.push_str(&pad);
            out.push_str("print ");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                match item {
                    PrintItem::Expr(e) => format_expr(e, false, out),
                    PrintItem::Str(s) => {
                        out.push('"');
                        out.push_str(&escape_string(s));
                        out.push('"');
                    }
                }
            }
            out.push_str(";\n");
        }
        Stmt::If { cond, then_body, else_body, span } => {
            out.push_str(&pad);
            out.push_str("if ");
            format_expr(cond, false, out);
            out.push_str(" {\n");
            for s in then_body {
                format_stmt(s, depth + 1, ctx, out);
            }
            // Drain comments inside the block tail. For an
            // if-without-else, before the closing `}`. For if/else,
            // before the `} else {` boundary, using the start of the
            // first else statement (or the parent's end if empty).
            if else_body.is_empty() {
                ctx.drain_to(span.end, depth + 1, out);
                out.push_str(&pad);
                out.push_str("}\n");
            } else {
                let else_start = else_body
                    .first()
                    .map(|s| s.span().start)
                    .unwrap_or(span.end);
                ctx.drain_to(else_start, depth + 1, out);
                out.push_str(&pad);
                out.push_str("} else {\n");
                for s in else_body {
                    format_stmt(s, depth + 1, ctx, out);
                }
                ctx.drain_to(span.end, depth + 1, out);
                out.push_str(&pad);
                out.push_str("}\n");
            }
        }
        Stmt::While { cond, invariants, body, span } => {
            out.push_str(&pad);
            out.push_str("while ");
            format_expr(cond, false, out);
            out.push('\n');
            for inv in invariants {
                out.push_str(&pad);
                out.push_str("invariant ");
                format_expr(inv, false, out);
                out.push_str(";\n");
            }
            out.push_str(&pad);
            out.push_str("{\n");
            for s in body {
                format_stmt(s, depth + 1, ctx, out);
            }
            ctx.drain_to(span.end, depth + 1, out);
            out.push_str(&pad);
            out.push_str("}\n");
        }
        Stmt::Assign { name, expr, .. } => {
            out.push_str(&pad);
            out.push_str(name);
            out.push_str(" = ");
            format_expr(expr, false, out);
            out.push_str(";\n");
        }
        Stmt::Break { .. } => {
            out.push_str(&pad);
            out.push_str("break;\n");
        }
        Stmt::Continue { .. } => {
            out.push_str(&pad);
            out.push_str("continue;\n");
        }
        Stmt::IndexAssign { name, index, field_path, value, .. } => {
            out.push_str(&pad);
            out.push_str(name);
            out.push('[');
            format_expr(index, false, out);
            out.push(']');
            for f in field_path {
                out.push('.');
                out.push_str(f);
            }
            out.push_str(" = ");
            format_expr(value, false, out);
            out.push_str(";\n");
        }
        Stmt::FieldAssign { object, field, value, .. } => {
            out.push_str(&pad);
            format_expr(object, true, out);
            out.push('.');
            out.push_str(field);
            out.push_str(" = ");
            format_expr(value, false, out);
            out.push_str(";\n");
        }
        Stmt::For { var, start, end, invariants, body, span, parallel, reductions } => {
            out.push_str(&pad);
            if *parallel {
                out.push_str("parallel ");
            }
            out.push_str("for ");
            out.push_str(var);
            out.push_str(" from ");
            format_expr(start, false, out);
            out.push_str(" to ");
            format_expr(end, false, out);
            out.push('\n');
            for inv in invariants {
                out.push_str(&pad);
                out.push_str("invariant ");
                format_expr(inv, false, out);
                out.push_str(";\n");
            }
            for r in reductions {
                out.push_str(&pad);
                out.push_str("reduce ");
                out.push_str(&r.var);
                out.push_str(" with ");
                out.push_str(r.op.display_symbol());
                out.push_str(";\n");
            }
            out.push_str(&pad);
            out.push_str("{\n");
            for s in body {
                format_stmt(s, depth + 1, ctx, out);
            }
            ctx.drain_to(span.end, depth + 1, out);
            out.push_str(&pad);
            out.push_str("}\n");
        }
        Stmt::ForIter { var, collection, consumes, body, span } => {
            out.push_str(&pad);
            out.push_str("for ");
            out.push_str(var);
            out.push_str(" in ");
            if !*consumes {
                out.push_str("ref ");
            }
            out.push_str(collection);
            out.push_str(" {\n");
            for s in body {
                format_stmt(s, depth + 1, ctx, out);
            }
            ctx.drain_to(span.end, depth + 1, out);
            out.push_str(&pad);
            out.push_str("}\n");
        }
        Stmt::TaskSpawn { name, body, span } => {
            out.push_str(&pad);
            out.push_str("task ");
            out.push_str(name);
            out.push_str(" {\n");
            for s in body {
                format_stmt(s, depth + 1, ctx, out);
            }
            ctx.drain_to(span.end, depth + 1, out);
            out.push_str(&pad);
            out.push_str("}\n");
        }
        Stmt::TaskJoin { name, .. } => {
            out.push_str(&pad);
            out.push_str("join ");
            out.push_str(name);
            out.push_str(";\n");
        }
    }
    // Single attach point covering every statement variant: if the
    // next pending comment lies on the same source line as the end
    // of this statement, append it as a trailing comment on the
    // last emitted line.
    ctx.try_attach_trailing(s.span().end, out);
}

/// Pretty-print an expression. `parens_if_binary` wraps binary
/// subexpressions in parentheses — the caller sets it whenever the
/// surrounding context binds tighter than `+`/`-`/comparisons, so we
/// over-parenthesize rather than try to track precedence. Re-parsing
/// is unaffected: `(a + b)` and `a + b` produce identical AST nodes.
fn format_expr(e: &Expr, parens_if_binary: bool, out: &mut String) {
    match &e.kind {
        ExprKind::Int(v) => out.push_str(&v.to_string()),
        ExprKind::Float(v) => {
            // {:?} gives a round-trippable representation
            // (e.g. `1.0`, not `1`).
            out.push_str(&format!("{:?}", v));
        }
        ExprKind::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        ExprKind::Str(s) => {
            out.push('"');
            out.push_str(&escape_string(s));
            out.push('"');
        }
        ExprKind::Var(name) => out.push_str(name),
        ExprKind::Unary { op, expr } => {
            out.push_str(match op {
                UnaryOp::Neg => "-",
                UnaryOp::Not => "!",
            });
            format_expr(expr, true, out);
        }
        ExprKind::Binary { op, left, right } => {
            if parens_if_binary {
                out.push('(');
            }
            format_expr(left, true, out);
            out.push(' ');
            out.push_str(op.display_symbol());
            out.push(' ');
            format_expr(right, true, out);
            if parens_if_binary {
                out.push(')');
            }
        }
        ExprKind::Call { name, args, .. } => {
            out.push_str(name);
            out.push('(');
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                format_expr(a, false, out);
            }
            out.push(')');
        }
        ExprKind::MethodCall { receiver, method, args, .. } => {
            format_expr(receiver, true, out);
            out.push('.');
            out.push_str(method);
            out.push('(');
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                format_expr(a, false, out);
            }
            out.push(')');
        }
        ExprKind::Cast { expr, ty } => {
            // `(e as T)` form. Inner needs parens if it's a binary.
            out.push('(');
            format_expr(expr, true, out);
            out.push_str(" as ");
            out.push_str(&format!("{}", ty));
            out.push(')');
        }
        ExprKind::ArrayLit { elements } => {
            out.push('[');
            for (i, el) in elements.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                format_expr(el, false, out);
            }
            out.push(']');
        }
        ExprKind::Index { array, index } => {
            format_expr(array, true, out);
            out.push('[');
            format_expr(index, false, out);
            out.push(']');
        }
        ExprKind::Len { array } => {
            out.push_str("len(");
            format_expr(array, false, out);
            out.push(')');
        }
        ExprKind::Ref { inner } => {
            out.push_str("ref ");
            format_expr(inner, true, out);
        }
        ExprKind::RefMut { inner } => {
            out.push_str("mut ref ");
            format_expr(inner, true, out);
        }
        ExprKind::Tuple(elements) => {
            out.push('(');
            for (i, el) in elements.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                format_expr(el, false, out);
            }
            out.push(')');
        }
        ExprKind::TupleAccess { tuple, index } => {
            format_expr(tuple, true, out);
            out.push('.');
            out.push_str(&index.to_string());
        }
        ExprKind::StructLit { type_name, fields, .. } => {
            out.push_str(type_name);
            out.push_str(" { ");
            for (i, (n, e)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(n);
                out.push_str(": ");
                format_expr(e, false, out);
            }
            out.push_str(" }");
        }
        ExprKind::FieldAccess { object, field } => {
            format_expr(object, true, out);
            out.push('.');
            out.push_str(field);
        }
        ExprKind::Match { scrutinee, arms } => {
            out.push_str("match ");
            format_expr(scrutinee, false, out);
            out.push_str(" {");
            for arm in arms {
                out.push(' ');
                match &arm.pattern {
                    crate::ast::Pattern::Variant { enum_name, variant } => {
                        out.push_str(enum_name);
                        out.push('.');
                        out.push_str(variant);
                    }
                    crate::ast::Pattern::VariantWithBinding { enum_name, variant, binding } => {
                        out.push_str(enum_name);
                        out.push('.');
                        out.push_str(variant);
                        out.push('(');
                        out.push_str(binding);
                        out.push(')');
                    }
                    crate::ast::Pattern::Int(v) => {
                        out.push_str(&v.to_string());
                    }
                    crate::ast::Pattern::Bool(b) => {
                        out.push_str(if *b { "true" } else { "false" });
                    }
                    crate::ast::Pattern::Str(s) => {
                        out.push('"');
                        out.push_str(s);
                        out.push('"');
                    }
                    crate::ast::Pattern::Wildcard => {
                        out.push('_');
                    }
                }
                out.push_str(" then ");
                format_expr(&arm.body, false, out);
                out.push(',');
            }
            out.push_str(" }");
        }
        ExprKind::IfExpr { cond, then_value, else_value } => {
            out.push_str("if ");
            format_expr(cond, false, out);
            out.push_str(" { ");
            format_expr(then_value, false, out);
            out.push_str(" } else { ");
            format_expr(else_value, false, out);
            out.push_str(" }");
        }
        ExprKind::Block { stmts, tail } => {
            // Inline block-expression form. Multi-statement
            // blocks render on one line so the round-trip
            // (parse → format → parse) stays stable; users
            // who want indented layout can format manually.
            // Comments inside the block aren't preserved
            // here — block-expr is usually short enough that
            // matters less than for top-level fn bodies.
            out.push_str("{ ");
            let empty_comments: &[crate::lexer::Comment] = &[];
            for s in stmts {
                let mut tmp = String::new();
                let mut ctx = FmtCtx::new("", empty_comments);
                format_stmt(s, 0, &mut ctx, &mut tmp);
                out.push_str(tmp.trim());
                out.push(' ');
            }
            format_expr(tail, false, out);
            out.push_str(" }");
        }
        ExprKind::Try { inner } => {
            out.push_str("try ");
            format_expr(inner, true, out);
        }
    }
}

fn escape_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;
    use crate::parser::parse;

    fn round_trip(src: &str) {
        let tokens = lex(src).expect("lex");
        let (program, diags) = parse(tokens);
        assert!(diags.is_empty(), "parse errors: {:?}", diags);
        let formatted = format_program(&program);
        let tokens2 = lex(&formatted).unwrap_or_else(|e| {
            panic!("re-lex failed on:\n{}\nerror: {:?}", formatted, e);
        });
        let (program2, diags2) = parse(tokens2);
        assert!(
            diags2.is_empty(),
            "re-parse errors on:\n{}\nerrors: {:?}",
            formatted,
            diags2
        );
        assert_eq!(
            strip_spans(&program),
            strip_spans(&program2),
            "AST changed across format round-trip:\n--- input ---\n{}\n--- formatted ---\n{}\n",
            src,
            formatted
        );
    }

    /// Span equality is irrelevant for round-trip — only the
    /// structural shape of the AST must match. PartialEq on Program
    /// compares spans too, so blank them via the Debug form with a
    /// regex would be brittle; instead we render the AST sans spans.
    fn strip_spans(p: &Program) -> String {
        // Cheap approach: clone into a fresh Program with all spans
        // replaced by Span::new(0, 0). PartialEq then compares only
        // structure. Implemented inline since we own all the types.
        fn z() -> crate::span::Span {
            crate::span::Span::new(0, 0)
        }
        let mut out = Program {
            intents: p.intents.clone(),
            functions: p.functions.clone(),
            uses: p.uses.clone(),
            structs: p.structs.clone(),
            enums: p.enums.clone(),
            interfaces: p.interfaces.clone(),
            impls: p.impls.clone(),
            consts: p.consts.clone(),
            type_aliases: p.type_aliases.clone(),
            methods_blocks: p.methods_blocks.clone(),
        };
        for u in &mut out.uses {
            u.span = z();
        }
        for i in &mut out.intents {
            i.span = z();
        }
        fn zero_function(f: &mut Function) {
            f.span = crate::span::Span::new(0, 0);
            for w in &mut f.where_clauses {
                w.span = crate::span::Span::new(0, 0);
            }
            for p in &mut f.params {
                p.span = crate::span::Span::new(0, 0);
                p.name_span = crate::span::Span::new(0, 0);
            }
            for e in &mut f.requires {
                zero_expr(e);
            }
            for e in &mut f.ensures {
                zero_expr(e);
            }
            zero_stmts(&mut f.body);
        }
        for f in &mut out.functions {
            zero_function(f);
        }
        for s in &mut out.structs {
            s.span = z();
            s.name_span = z();
            for f in &mut s.fields {
                f.span = z();
            }
        }
        for e in &mut out.enums {
            e.span = z();
            e.name_span = z();
            for v in &mut e.variants {
                v.name_span = z();
            }
        }
        for ifc in &mut out.interfaces {
            ifc.span = z();
            ifc.name_span = z();
            for m in &mut ifc.methods {
                m.span = z();
                m.name_span = z();
                for p in &mut m.params {
                    p.span = z();
                    p.name_span = z();
                }
            }
        }
        for im in &mut out.impls {
            im.span = z();
            for m in &mut im.methods {
                zero_function(m);
            }
        }
        for c in &mut out.consts {
            c.span = z();
            c.name_span = z();
            zero_expr(&mut c.value);
        }
        for a in &mut out.type_aliases {
            a.span = z();
            a.name_span = z();
        }
        for m in &mut out.methods_blocks {
            m.span = z();
            m.for_type_span = z();
            for method in &mut m.methods {
                zero_function(method);
            }
        }
        format!("{:#?}", out)
    }

    fn zero_expr(e: &mut Expr) {
        e.span = crate::span::Span::new(0, 0);
        match &mut e.kind {
            ExprKind::Unary { expr, .. } => zero_expr(expr),
            ExprKind::Binary { left, right, .. } => {
                zero_expr(left);
                zero_expr(right);
            }
            ExprKind::Call { args, name_span, .. } => {
                *name_span = crate::span::Span::new(0, 0);
                for a in args {
                    zero_expr(a);
                }
            }
            ExprKind::MethodCall { receiver, method_span, args, .. } => {
                zero_expr(receiver);
                *method_span = crate::span::Span::new(0, 0);
                for a in args {
                    zero_expr(a);
                }
            }
            ExprKind::Cast { expr, .. } => zero_expr(expr),
            ExprKind::ArrayLit { elements } => {
                for el in elements {
                    zero_expr(el);
                }
            }
            ExprKind::Index { array, index } => {
                zero_expr(array);
                zero_expr(index);
            }
            ExprKind::Len { array } => zero_expr(array),
            ExprKind::Ref { inner } | ExprKind::RefMut { inner } => zero_expr(inner),
            ExprKind::FieldAccess { object, .. } => zero_expr(object),
            ExprKind::TupleAccess { tuple, .. } => zero_expr(tuple),
            ExprKind::Tuple(elements) => {
                for e in elements {
                    zero_expr(e);
                }
            }
            ExprKind::StructLit { type_name_span, fields, .. } => {
                *type_name_span = crate::span::Span::new(0, 0);
                for (_, e) in fields {
                    zero_expr(e);
                }
            }
            ExprKind::Match { scrutinee, arms } => {
                zero_expr(scrutinee);
                for arm in arms {
                    arm.pattern_span = crate::span::Span::new(0, 0);
                    zero_expr(&mut arm.body);
                }
            }
            ExprKind::IfExpr { cond, then_value, else_value } => {
                zero_expr(cond);
                zero_expr(then_value);
                zero_expr(else_value);
            }
            _ => {}
        }
    }

    fn zero_stmts(ss: &mut Vec<Stmt>) {
        for s in ss {
            match s {
                Stmt::Let { expr, span, .. } => {
                    zero_expr(expr);
                    *span = crate::span::Span::new(0, 0);
                }
                Stmt::LetTuple { expr, span, .. } => {
                    zero_expr(expr);
                    *span = crate::span::Span::new(0, 0);
                }
                Stmt::Return { expr, span } => {
                    zero_expr(expr);
                    *span = crate::span::Span::new(0, 0);
                }
                Stmt::Assert { expr, span, .. } => {
                    zero_expr(expr);
                    *span = crate::span::Span::new(0, 0);
                }
                Stmt::Prove { expr, span } => {
                    zero_expr(expr);
                    *span = crate::span::Span::new(0, 0);
                }
                Stmt::Print { items, span } => {
                    for it in items {
                        if let PrintItem::Expr(e) = it {
                            zero_expr(e);
                        }
                    }
                    *span = crate::span::Span::new(0, 0);
                }
                Stmt::If { cond, then_body, else_body, span } => {
                    zero_expr(cond);
                    zero_stmts(then_body);
                    zero_stmts(else_body);
                    *span = crate::span::Span::new(0, 0);
                }
                Stmt::While { cond, invariants, body, span } => {
                    zero_expr(cond);
                    for e in invariants {
                        zero_expr(e);
                    }
                    zero_stmts(body);
                    *span = crate::span::Span::new(0, 0);
                }
                Stmt::Assign { expr, span, .. } => {
                    zero_expr(expr);
                    *span = crate::span::Span::new(0, 0);
                }
                Stmt::Break { span } | Stmt::Continue { span } => {
                    *span = crate::span::Span::new(0, 0);
                }
                Stmt::IndexAssign { index, value, span, .. } => {
                    zero_expr(index);
                    zero_expr(value);
                    *span = crate::span::Span::new(0, 0);
                }
                Stmt::FieldAssign { object, value, field_span, span, .. } => {
                    zero_expr(object);
                    zero_expr(value);
                    *field_span = crate::span::Span::new(0, 0);
                    *span = crate::span::Span::new(0, 0);
                }
                Stmt::For { start, end, invariants, body, span, .. } => {
                    zero_expr(start);
                    zero_expr(end);
                    for e in invariants {
                        zero_expr(e);
                    }
                    zero_stmts(body);
                    *span = crate::span::Span::new(0, 0);
                }
                Stmt::ForIter { body, span, .. } => {
                    zero_stmts(body);
                    *span = crate::span::Span::new(0, 0);
                }
                Stmt::TaskSpawn { body, span, .. } => {
                    zero_stmts(body);
                    *span = crate::span::Span::new(0, 0);
                }
                Stmt::TaskJoin { span, .. } => {
                    *span = crate::span::Span::new(0, 0);
                }
            }
        }
    }

    fn fmt_with_comments(src: &str) -> String {
        let tokens = lex(src).expect("lex");
        let (program, diags) = parse(tokens);
        assert!(diags.is_empty(), "parse errors: {:?}", diags);
        let comments = crate::lexer::extract_comments(src);
        format_program_with_comments(&program, src, &comments)
    }

    #[test]
    fn preserves_leading_comment_above_function() {
        let src = "// preamble\nfn main() -> i64 { return 0; }\n";
        let out = fmt_with_comments(src);
        assert!(out.contains("// preamble\n"), "got:\n{out}");
        assert!(out.contains("fn main() -> i64"), "got:\n{out}");
    }

    #[test]
    fn preserves_comment_between_statements() {
        let src = "fn main() -> i64 {\n  let x: i64 = 1;\n  // bumping\n  let y: i64 = x + 1;\n  return y;\n}\n";
        let out = fmt_with_comments(src);
        // The interstitial comment must appear after the first let
        // and before the second.
        let first = out.find("let x: i64 = 1").expect("first let");
        let cmt = out.find("// bumping").expect("comment kept");
        let second = out.find("let y: i64 = ").expect("second let");
        assert!(first < cmt && cmt < second, "order wrong:\n{out}");
    }

    #[test]
    fn preserves_trailing_comment_inside_function() {
        // A comment placed just before the closing `}` should be
        // drained at the body's indent, not promoted out of the
        // function.
        let src = "fn main() -> i64 {\n  return 0;\n  // trailing\n}\n";
        let out = fmt_with_comments(src);
        let cmt = out.find("// trailing").expect("comment kept");
        let close = out.rfind("}\n").expect("brace");
        assert!(cmt < close, "comment should be inside body:\n{out}");
        // And at indent 2 (the body's indent).
        let line_start = out[..cmt].rfind('\n').map(|i| i + 1).unwrap_or(0);
        assert_eq!(&out[line_start..line_start + 2], "  ", "indent off:\n{out}");
    }

    #[test]
    fn fmt_is_idempotent_for_simple_program_with_comments() {
        let src = "// hi\nfn main() -> i64 { return 0; }\n";
        let once = fmt_with_comments(src);
        let twice = fmt_with_comments(&once);
        assert_eq!(once, twice, "fmt should be idempotent");
    }

    #[test]
    fn preserves_blank_line_between_comment_groups() {
        // Two comment paragraphs separated by a blank line should
        // stay separated in the output.
        let src = "// first\n//\n// second\nfn main() -> i64 {\n  return 0;\n}\n";
        let out = fmt_with_comments(src);
        assert!(out.contains("// first\n//\n// second"), "got:\n{out}");
    }

    #[test]
    fn preserves_blank_line_before_function() {
        // Source has a blank line between a comment block and the
        // function. Output should mirror it.
        let src = "// preamble\n\nfn main() -> i64 {\n  return 0;\n}\n";
        let out = fmt_with_comments(src);
        assert!(
            out.contains("// preamble\n\nfn main()"),
            "expected blank between comment and fn, got:\n{out}"
        );
    }

    #[test]
    fn preserves_blank_line_between_statements() {
        // Source has a blank line between two statements. Output
        // should keep it as a visual grouping marker.
        let src = "fn main() -> i64 {\n  let x: i64 = 1;\n\n  let y: i64 = 2;\n  return x + y;\n}\n";
        let out = fmt_with_comments(src);
        assert!(
            out.contains("let x: i64 = 1;\n\n  let y"),
            "expected blank between lets, got:\n{out}"
        );
    }

    #[test]
    fn preserves_same_line_trailing_comment() {
        // A `// …` on the same source line as a `;` should remain
        // trailing, not get promoted to a leading comment on the
        // next statement.
        let src = "fn main() -> i64 {\n  let x: i64 = 1;  // bound to one\n  return x;\n}\n";
        let out = fmt_with_comments(src);
        assert!(
            out.contains("let x: i64 = 1;  // bound to one\n"),
            "expected trailing comment on the let line, got:\n{out}"
        );
        // And NOT promoted onto its own leading line above the
        // return. The exact bytes `// bound to one\n  return`
        // naturally appear when the comment is trailing (its line
        // ends, then `return` follows), so the check is for a
        // *leading-indent + comment* line:
        assert!(
            !out.contains("\n  // bound to one\n"),
            "comment was wrongly promoted to leading, got:\n{out}"
        );
    }

    #[test]
    fn formats_simple_function() {
        round_trip("fn main() -> i64 { return 42; }");
    }

    #[test]
    fn formats_function_with_contracts() {
        round_trip(
            r#"fn f(x: i64) -> i64
            requires x > 0;
            ensures _return > 0;
            { return x + 1; }
            fn main() -> i64 { return f(1); }"#,
        );
    }

    #[test]
    fn formats_print_with_items() {
        round_trip(r#"fn main() -> i64 { print "x=", 1, "y=", 2; return 0; }"#);
    }

    #[test]
    fn formats_while_with_invariants() {
        round_trip(
            r#"fn main() -> i64 {
              let i: i64 = 0;
              while i < 10
              invariant i >= 0;
              invariant i <= 10;
              { i = i + 1; }
              return i;
            }"#,
        );
    }

    #[test]
    fn formats_for_iter_borrow_and_consume() {
        round_trip(
            r#"fn main() -> i64 {
              let xs: [i64; 3] = [1, 2, 3];
              let s: i64 = 0;
              for x in ref xs { s = s + x; }
              return s;
            }"#,
        );
    }

    #[test]
    fn formats_generic_fn_type_params() {
        // T1.4 phase 1 surface: `fn id<T>(x: T) -> T { … }`
        // must round-trip through the formatter — `<T>` is
        // emitted right after the fn name, before the param
        // list.
        round_trip("fn id<T>(x: T) -> T { return x; }");
    }

    #[test]
    fn formats_generic_fn_multi_type_params() {
        round_trip("fn pair<A, B>(a: A, b: B) -> A { return a; }");
    }

    #[test]
    fn formats_where_bound() {
        // T1.5 phase 1 surface: `where T is Cmp` after the
        // return type round-trips. (Even though the checker
        // gates it as WIP, the formatter must preserve the
        // shape for future phase-2 work.)
        round_trip("fn pick<T>(x: T, y: T) -> T where T is Cmp { return x; }");
    }

    #[test]
    fn formats_where_multi_bound() {
        round_trip(
            "fn run<A, B>(a: A, b: B) -> A where A is Cmp, B is Hash { return a; }",
        );
    }

    #[test]
    fn formats_struct_decl_round_trip() {
        // T1.2 phase 1 surface: top-level struct decls must
        // survive the formatter — previously they were silently
        // dropped from the output.
        round_trip(
            r#"struct Point { x: i64, y: i64, }
fn main() -> i64 { return 0; }"#,
        );
    }

    #[test]
    fn formats_enum_decl_payload_less_round_trip() {
        // T1.3 phase 1 surface.
        round_trip(
            r#"enum Color { Red, Green, Blue, }
fn main() -> i64 { return 0; }"#,
        );
    }

    #[test]
    fn formats_enum_decl_with_payload_round_trip() {
        // T1.3 phase 2a surface: `Some(T)` style payloads
        // survive the formatter even though the codegen gate
        // hasn't lifted yet — the surface syntax must
        // round-trip cleanly so future phase-2b work can
        // build on a stable parse-format invariant.
        round_trip(
            r#"enum Maybe { Some(i64), None, }
fn main() -> i64 { return 0; }"#,
        );
    }

    #[test]
    fn formats_interface_decl_round_trip() {
        // T1.5 phase 1 surface.
        round_trip(
            r#"interface Show { fn show(self: i64) -> i64; }
fn main() -> i64 { return 0; }"#,
        );
    }

    #[test]
    fn formats_if_expression_round_trip() {
        round_trip(
            r#"fn main() -> i64 {
  let r: i64 = if true { 10 } else { 20 };
  return r;
}"#,
        );
    }

    #[test]
    fn formats_field_assign_round_trip() {
        round_trip(
            r#"struct Point { x: i64, y: i64, }
fn main() -> i64 {
  let p: Point = Point { x: 1, y: 2 };
  p.x = 10;
  return p.x + p.y;
}"#,
        );
    }

    #[test]
    fn formats_pure_fn_method_round_trip() {
        round_trip(
            r#"struct Point { x: i64, y: i64, }
methods on Point {
  pure fn dist(self: Point) -> i64 { return self.x + self.y; }
}
fn main() -> i64 { return 0; }"#,
        );
    }

    #[test]
    fn formats_method_chain_round_trip() {
        round_trip(
            r#"struct Point { x: i64, y: i64, }
methods on Point {
  fn shift(self: Point, dx: i64) -> Point {
    return Point { x: self.x + dx, y: self.y };
  }
  fn x_val(self: Point) -> i64 { return self.x; }
}
fn main() -> i64 { return Point { x: 1, y: 2 }.shift(5).shift(2).x_val(); }"#,
        );
    }

    #[test]
    fn formats_if_expression_else_if_chain_round_trip() {
        // else-if chains parse as nested if-expressions
        // on the else side; format emits them as a single
        // nested expression, which re-parses to the same
        // structural AST.
        round_trip(
            r#"fn main() -> i64 {
  let r: i64 = if true { 1 } else if false { 2 } else { 3 };
  return r;
}"#,
        );
    }

    #[test]
    fn formats_integer_match_pattern_round_trip() {
        // T1.3 integer pattern: `<int>` and `-<int>` arms
        // must round-trip through the formatter.
        round_trip(
            r#"fn describe(x: i64) -> i64 {
  return match x { 0 then 1, 42 then 2, -1 then 3, _ then 0, };
}
fn main() -> i64 { return describe(42); }"#,
        );
    }

    #[test]
    fn formats_methods_block_round_trip() {
        // T1.2 phase 2a: top-level `methods on T { … }`
        // blocks must round-trip through the formatter.
        round_trip(
            r#"struct Point { x: i64, y: i64, }
methods on Point {
  fn manhattan(self: Point) -> i64 { return self.x + self.y; }
}
fn main() -> i64 { return 0; }"#,
        );
    }

    #[test]
    fn formats_method_call_round_trip() {
        // `recv.method(args)` must survive the formatter.
        round_trip(
            r#"struct Point { x: i64, y: i64, }
methods on Point {
  fn shift(self: Point, dx: i64) -> i64 { return self.x + dx; }
}
fn main() -> i64 {
  let p: Point = Point { x: 1, y: 2 };
  return p.shift(5);
}"#,
        );
    }

    #[test]
    fn formats_type_alias_round_trip() {
        // T4.15: top-level `type Name = Type;` aliases
        // must survive the formatter (alias-resolution
        // happens at check time, not format time).
        round_trip(
            r#"type Coord = (i64, i64);
type Score = i64;
fn main() -> i64 { return 0; }"#,
        );
    }

    #[test]
    fn formats_const_decl_round_trip() {
        // T4.15: top-level `const NAME: T = literal;` decls
        // must survive the formatter.
        round_trip(
            r#"const PI: f64 = 3.14;
const ANSWER: i64 = 42;
fn main() -> i64 { return ANSWER; }"#,
        );
    }

    #[test]
    fn formats_impl_decl_round_trip() {
        // T1.5 phase 1 surface.
        round_trip(
            r#"interface Show { fn show(self: i64) -> i64; }
implement Show for i64 { fn show(self: i64) -> i64 { return self; } }
fn main() -> i64 { return 0; }"#,
        );
    }
}
