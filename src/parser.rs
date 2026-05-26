use crate::ast::{
    BinaryOp, ConstDecl, EnumDecl, EnumVariant, Expr, ExprKind, Function, ImplDecl, Intent,
    InterfaceDecl, InterfaceMethod, MatchArm, MethodsBlock, Param, Pattern, Program, Reduction,
    ReductionOp, Stmt, StructDecl, StructField, Type, TypeAlias, UnaryOp, Use, WhereClause,
};

/// Parser-internal sum of the two `use`-statement shapes
/// (closure #245). The top-level parse loop dispatches each
/// variant to the matching list on `Program`.
enum UseDecl {
    File(Use),
    Path(crate::ast::UsePath),
}
use crate::diagnostic::Diagnostic;
use crate::lexer::{Token, TokenKind};

pub fn parse(tokens: Vec<Token>) -> (Program, Vec<Diagnostic>) {
    let mut parser = Parser::new(tokens);
    let program = parser.parse_program();
    (program, parser.errors)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    errors: Vec<Diagnostic>,
    /// Names of type parameters declared on the function
    /// currently being parsed (e.g. `["T", "U"]` for
    /// `fn pair<T, U>(...)`). `parse_type` consults this so a
    /// bare uppercase identifier resolves to `Type::Param`
    /// instead of `Type::Struct`. Refines T1.4.
    current_type_params: std::collections::HashSet<String>,
    /// Const declarations seen so far in the source, mapped
    /// to their literal i128 value. Populated by
    /// `parse_const_decl` when the initializer is an integer
    /// literal (including negative literals via the `Minus`
    /// prefix). Consulted by `parse_type` when an identifier
    /// appears in an array-length slot (`[T; SIZE]`). Forward
    /// references and non-literal const initializers aren't
    /// supported here. T0.0 follow-up (closure #120).
    const_int_values: std::collections::HashMap<String, i128>,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self {
            tokens,
            pos: 0,
            errors: Vec::new(),
            current_type_params: std::collections::HashSet::new(),
            const_int_values: std::collections::HashMap::new(),
        }
    }

    fn parse_program(&mut self) -> Program {
        let mut intents = Vec::new();
        let mut functions = Vec::new();
        let mut uses = Vec::new();
        let mut use_paths: Vec<crate::ast::UsePath> = Vec::new();
        let mut structs = Vec::new();
        let mut enums = Vec::new();
        let mut interfaces = Vec::new();
        let mut impls = Vec::new();
        let mut consts = Vec::new();
        let mut type_aliases = Vec::new();
        let mut methods_blocks = Vec::new();
        let mut modules: Vec<crate::ast::ModuleDecl> = Vec::new();

        while !self.check(|kind| matches!(kind, TokenKind::Eof)) {
            // Closure #242: module declarations. v1 supports
            // only top-level modules (no nesting). Items inside
            // are stored in the ModuleDecl; the checker walks
            // them later and mangles names + enforces
            // visibility.
            if self.check(|k| matches!(k, TokenKind::Module)) {
                match self.parse_module_decl() {
                    Ok(m) => modules.push(m),
                    Err(e) => {
                        self.errors.push(e);
                        self.sync_to_top_level();
                    }
                }
                continue;
            }
            if self.check(|kind| matches!(kind, TokenKind::Intent)) {
                match self.parse_intent() {
                    Ok(i) => intents.push(i),
                    Err(e) => {
                        self.errors.push(e);
                        self.sync_to_top_level();
                    }
                }
            } else if self.check(|kind| matches!(kind, TokenKind::Use)) {
                match self.parse_use() {
                    Ok(UseDecl::File(u)) => uses.push(u),
                    Ok(UseDecl::Path(p)) => use_paths.push(p),
                    Err(e) => {
                        self.errors.push(e);
                        self.sync_to_top_level();
                    }
                }
            } else if self.check(|k| matches!(k, TokenKind::Struct)) {
                match self.parse_struct_decl() {
                    Ok(s) => structs.push(s),
                    Err(e) => {
                        self.errors.push(e);
                        self.sync_to_top_level();
                    }
                }
            } else if self.check(|k| matches!(k, TokenKind::Enum)) {
                match self.parse_enum_decl() {
                    Ok(e) => enums.push(e),
                    Err(err) => {
                        self.errors.push(err);
                        self.sync_to_top_level();
                    }
                }
            } else if self.check(|k| matches!(k, TokenKind::Interface)) {
                match self.parse_interface_decl() {
                    Ok(d) => interfaces.push(d),
                    Err(err) => {
                        self.errors.push(err);
                        self.sync_to_top_level();
                    }
                }
            } else if self.check(|k| matches!(k, TokenKind::Implement)) {
                match self.parse_impl_decl() {
                    Ok(d) => impls.push(d),
                    Err(err) => {
                        self.errors.push(err);
                        self.sync_to_top_level();
                    }
                }
            } else if self.check(|k| matches!(k, TokenKind::Const)) {
                match self.parse_const_decl() {
                    Ok(c) => consts.push(c),
                    Err(err) => {
                        self.errors.push(err);
                        self.sync_to_top_level();
                    }
                }
            } else if self.check(|k| matches!(k, TokenKind::Type)) {
                match self.parse_type_alias() {
                    Ok(a) => type_aliases.push(a),
                    Err(err) => {
                        self.errors.push(err);
                        self.sync_to_top_level();
                    }
                }
            } else if self.check(|k| matches!(k, TokenKind::Methods)) {
                match self.parse_methods_block() {
                    Ok(m) => methods_blocks.push(m),
                    Err(err) => {
                        self.errors.push(err);
                        self.sync_to_top_level();
                    }
                }
            } else if self.check(|kind| matches!(kind, TokenKind::Fn | TokenKind::Pure)) {
                match self.parse_function() {
                    Ok(f) => functions.push(f),
                    Err(e) => {
                        self.errors.push(e);
                        self.sync_to_top_level();
                    }
                }
            } else {
                let err = self.error_here("expected 'use', 'intent', 'struct', or 'fn'");
                self.errors.push(err);
                if !self.check(|kind| matches!(kind, TokenKind::Eof)) {
                    self.bump();
                }
                self.sync_to_top_level();
            }
        }

        Program {
            intents,
            functions,
            uses,
            structs,
            enums,
            interfaces,
            impls,
            consts,
            type_aliases,
            methods_blocks,
            modules,
            use_paths,
        }
    }

    /// Closure #242: parse a `module name { items… }` block.
    /// Items inside follow the same grammar as top-level items;
    /// each can be prefixed with `pub` to export. v1 forbids
    /// nested `module` declarations.
    fn parse_module_decl(&mut self) -> Result<crate::ast::ModuleDecl, Diagnostic> {
        let start = self.expect_keyword("'module'", |k| matches!(k, TokenKind::Module))?;
        let name_tok = self.expect_ident()?;
        let name_span = name_tok.span;
        let name = ident_text(name_tok);
        self.expect_keyword("'{' after module name", |k| matches!(k, TokenKind::LBrace))?;

        let mut functions = Vec::new();
        let mut structs = Vec::new();
        let mut enums = Vec::new();
        let mut interfaces = Vec::new();
        let mut impls = Vec::new();
        let mut consts = Vec::new();
        let mut type_aliases = Vec::new();
        let mut methods_blocks = Vec::new();
        let mut vis = crate::ast::ModuleVisibility::default();

        while !self.check(|k| matches!(k, TokenKind::RBrace | TokenKind::Eof)) {
            // Reject nested modules (v1 doesn't support them).
            if self.check(|k| matches!(k, TokenKind::Module)) {
                let tok = self.bump();
                self.errors.push(Diagnostic::new(
                    tok.span,
                    "nested `module` declarations are not supported in v1; \
                     define modules only at the top level",
                ));
                self.sync_past_brace();
                continue;
            }
            // Optional `pub` modifier. Top-level item parsing
            // doesn't see `pub` today; inside a module it
            // declares visibility.
            let is_pub = self
                .match_token(|k| matches!(k, TokenKind::Pub))
                .is_some();

            if self.check(|k| matches!(k, TokenKind::Struct)) {
                match self.parse_struct_decl() {
                    Ok(s) => {
                        structs.push(s);
                        vis.structs_pub.push(is_pub);
                    }
                    Err(e) => { self.errors.push(e); self.sync_past_brace(); }
                }
            } else if self.check(|k| matches!(k, TokenKind::Enum)) {
                match self.parse_enum_decl() {
                    Ok(e) => {
                        enums.push(e);
                        vis.enums_pub.push(is_pub);
                    }
                    Err(e) => { self.errors.push(e); self.sync_past_brace(); }
                }
            } else if self.check(|k| matches!(k, TokenKind::Interface)) {
                match self.parse_interface_decl() {
                    Ok(d) => {
                        interfaces.push(d);
                        vis.interfaces_pub.push(is_pub);
                    }
                    Err(e) => { self.errors.push(e); self.sync_past_brace(); }
                }
            } else if self.check(|k| matches!(k, TokenKind::Implement)) {
                match self.parse_impl_decl() {
                    Ok(d) => {
                        impls.push(d);
                        vis.impls_pub.push(is_pub);
                    }
                    Err(e) => { self.errors.push(e); self.sync_past_brace(); }
                }
            } else if self.check(|k| matches!(k, TokenKind::Const)) {
                match self.parse_const_decl() {
                    Ok(c) => {
                        consts.push(c);
                        vis.consts_pub.push(is_pub);
                    }
                    Err(e) => { self.errors.push(e); self.sync_past_brace(); }
                }
            } else if self.check(|k| matches!(k, TokenKind::Type)) {
                match self.parse_type_alias() {
                    Ok(a) => {
                        type_aliases.push(a);
                        vis.type_aliases_pub.push(is_pub);
                    }
                    Err(e) => { self.errors.push(e); self.sync_past_brace(); }
                }
            } else if self.check(|k| matches!(k, TokenKind::Methods)) {
                match self.parse_methods_block() {
                    Ok(m) => {
                        methods_blocks.push(m);
                        vis.methods_blocks_pub.push(is_pub);
                    }
                    Err(e) => { self.errors.push(e); self.sync_past_brace(); }
                }
            } else if self.check(|k| matches!(k, TokenKind::Fn | TokenKind::Pure)) {
                match self.parse_function() {
                    Ok(f) => {
                        functions.push(f);
                        vis.functions_pub.push(is_pub);
                    }
                    Err(e) => { self.errors.push(e); self.sync_past_brace(); }
                }
            } else {
                let err = self.error_here(
                    "expected an item declaration (fn / struct / enum / interface / implement / methods / const / type) inside `module`"
                );
                self.errors.push(err);
                if !self.check(|kind| matches!(kind, TokenKind::Eof | TokenKind::RBrace)) {
                    self.bump();
                }
            }
        }

        let close_tok = self.expect_keyword(
            "'}' to close module",
            |k| matches!(k, TokenKind::RBrace),
        )?;
        Ok(crate::ast::ModuleDecl {
            name,
            name_span,
            functions,
            structs,
            enums,
            interfaces,
            impls,
            consts,
            type_aliases,
            methods_blocks,
            visibility: vis,
            span: start.span.merge(close_tok.span),
        })
    }

    /// Helper: skip tokens until past the next `}` (for error
    /// recovery inside a module body when a single item fails).
    fn sync_past_brace(&mut self) {
        let mut depth = 0i32;
        while !self.check(|k| matches!(k, TokenKind::Eof)) {
            if self.check(|k| matches!(k, TokenKind::LBrace)) {
                depth += 1;
            } else if self.check(|k| matches!(k, TokenKind::RBrace)) {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            self.bump();
        }
    }

    fn parse_methods_block(&mut self) -> Result<MethodsBlock, Diagnostic> {
        let start = self.expect_keyword("'methods'", |k| matches!(k, TokenKind::Methods))?;
        // `on` follows. It's not a reserved keyword (used
        // only in this syntax position), so accept it as an
        // identifier with the literal text "on".
        let on_tok = self.expect_ident()?;
        if ident_text(on_tok.clone()) != "on" {
            return Err(Diagnostic::new(
                on_tok.span,
                "expected 'on' in `methods on <Type> { … }`",
            ));
        }
        let ty_start_span = self.current().span;
        let for_type = self.parse_type()?;
        self.expect_keyword("'{'", |k| matches!(k, TokenKind::LBrace))?;
        let mut methods = Vec::new();
        while !self.check(|k| matches!(k, TokenKind::RBrace | TokenKind::Eof)) {
            methods.push(self.parse_function()?);
        }
        let close = self.expect_keyword("'}'", |k| matches!(k, TokenKind::RBrace))?;
        Ok(MethodsBlock {
            for_type,
            for_type_span: ty_start_span,
            methods,
            span: start.span.merge(close.span),
        })
    }

    fn parse_type_alias(&mut self) -> Result<TypeAlias, Diagnostic> {
        let start = self.expect_keyword("'type'", |k| matches!(k, TokenKind::Type))?;
        let name_tok = self.expect_ident()?;
        let name_span = name_tok.span;
        let name = ident_text(name_tok);
        self.expect_keyword("'='", |k| matches!(k, TokenKind::Equal))?;
        let target = self.parse_type()?;
        let semi = self.expect_keyword("';'", |k| matches!(k, TokenKind::Semicolon))?;
        Ok(TypeAlias {
            name,
            name_span,
            target,
            span: start.span.merge(semi.span),
        })
    }

    fn parse_const_decl(&mut self) -> Result<ConstDecl, Diagnostic> {
        let start = self.expect_keyword("'const'", |k| matches!(k, TokenKind::Const))?;
        let name_tok = self.expect_ident()?;
        let name_span = name_tok.span;
        let name = ident_text(name_tok);
        self.expect_keyword("':'", |k| matches!(k, TokenKind::Colon))?;
        let ty = self.parse_type()?;
        self.expect_keyword("'='", |k| matches!(k, TokenKind::Equal))?;
        let value = self.parse_expr()?;
        let semi = self.expect_keyword("';'", |k| matches!(k, TokenKind::Semicolon))?;
        // Stash the integer-valued initializer so `parse_type`
        // can resolve a later `[T; NAME]` length reference.
        // Only literal forms (`42`, `-1`) qualify. T0.0
        // follow-up (closure #120).
        if let Some(v) = expr_as_int_literal(&value, &self.const_int_values) {
            self.const_int_values.insert(name.clone(), v);
        }
        Ok(ConstDecl {
            name,
            name_span,
            ty,
            value,
            span: start.span.merge(semi.span),
        })
    }

    fn parse_interface_decl(&mut self) -> Result<InterfaceDecl, Diagnostic> {
        let start = self.expect_keyword("'interface'", |k| matches!(k, TokenKind::Interface))?;
        let name_tok = self.expect_ident()?;
        let name_span = name_tok.span;
        let name = ident_text(name_tok);
        self.expect_keyword("'{'", |k| matches!(k, TokenKind::LBrace))?;
        let mut methods = Vec::new();
        while !self.check(|k| matches!(k, TokenKind::RBrace | TokenKind::Eof)) {
            let fn_tok = self.expect_keyword("'fn'", |k| matches!(k, TokenKind::Fn))?;
            let m_name_tok = self.expect_ident()?;
            let m_name_span = m_name_tok.span;
            let m_name = ident_text(m_name_tok);
            self.expect_keyword("'('", |k| matches!(k, TokenKind::LParen))?;
            let mut params = Vec::new();
            if !self.check(|k| matches!(k, TokenKind::RParen)) {
                loop {
                    let p_tok = self.expect_ident()?;
                    let p_name_span = p_tok.span;
                    let p_name = ident_text(p_tok);
                    self.expect_keyword("':'", |k| matches!(k, TokenKind::Colon))?;
                    let ty = self.parse_type()?;
                    params.push(Param {
                        name: p_name,
                        ty,
                        name_span: p_name_span,
                        span: p_name_span,
                    });
                    if self.match_token(|k| matches!(k, TokenKind::Comma)).is_none() {
                        break;
                    }
                    if self.check(|k| matches!(k, TokenKind::RParen)) {
                        break;
                    }
                }
            }
            self.expect_keyword("')'", |k| matches!(k, TokenKind::RParen))?;
            self.expect_keyword(
                "'returns'", // checker uses display-only `returns`; parser still accepts `->`
                |k| matches!(k, TokenKind::Arrow),
            )?;
            let return_type = self.parse_type()?;
            let semi = self.expect_keyword("';'", |k| matches!(k, TokenKind::Semicolon))?;
            methods.push(InterfaceMethod {
                name: m_name,
                name_span: m_name_span,
                params,
                return_type,
                span: fn_tok.span.merge(semi.span),
            });
        }
        let close = self.expect_keyword("'}'", |k| matches!(k, TokenKind::RBrace))?;
        Ok(InterfaceDecl {
            name,
            name_span,
            methods,
            span: start.span.merge(close.span),
        })
    }

    fn parse_impl_decl(&mut self) -> Result<ImplDecl, Diagnostic> {
        let start = self.expect_keyword("'implement'", |k| matches!(k, TokenKind::Implement))?;
        let iface_tok = self.expect_ident()?;
        let interface_name = ident_text(iface_tok);
        // `for` is a reserved keyword (used in `for i from … to
        // …`), so dispatch on the token kind rather than trying
        // to grab it as an identifier.
        self.expect_keyword("'for' in `implement <Iface> for <Type>`", |k| {
            matches!(k, TokenKind::For)
        })?;
        let for_type = self.parse_type()?;
        self.expect_keyword("'{'", |k| matches!(k, TokenKind::LBrace))?;
        let mut methods = Vec::new();
        while !self.check(|k| matches!(k, TokenKind::RBrace | TokenKind::Eof)) {
            methods.push(self.parse_function()?);
        }
        let close = self.expect_keyword("'}'", |k| matches!(k, TokenKind::RBrace))?;
        Ok(ImplDecl {
            interface_name,
            for_type,
            methods,
            span: start.span.merge(close.span),
        })
    }

    fn parse_enum_decl(&mut self) -> Result<EnumDecl, Diagnostic> {
        let start = self.expect_keyword("'enum'", |k| matches!(k, TokenKind::Enum))?;
        let name_tok = self.expect_ident()?;
        let name_span = name_tok.span;
        let name = ident_text(name_tok);
        self.expect_keyword("'{'", |k| matches!(k, TokenKind::LBrace))?;
        let mut variants = Vec::new();
        while !self.check(|k| matches!(k, TokenKind::RBrace | TokenKind::Eof)) {
            let v_tok = self.expect_ident()?;
            let v_span = v_tok.span;
            let v_name = ident_text(v_tok);
            // Optional payload: `Name(T1, T2, …)` — types only,
            // positional. T1.3 phase 2a. Named fields (`Err {
            // code: i64, msg: String }`) land in phase 2b.
            let mut payload: Vec<Type> = Vec::new();
            if self
                .match_token(|k| matches!(k, TokenKind::LParen))
                .is_some()
            {
                if !self.check(|k| matches!(k, TokenKind::RParen)) {
                    loop {
                        let ty = self.parse_type()?;
                        payload.push(ty);
                        if self
                            .match_token(|k| matches!(k, TokenKind::Comma))
                            .is_none()
                        {
                            break;
                        }
                    }
                }
                self.expect_keyword("')'", |k| matches!(k, TokenKind::RParen))?;
            }
            let comma_seen = self
                .match_token(|k| matches!(k, TokenKind::Comma))
                .is_some();
            variants.push(EnumVariant {
                name: v_name,
                name_span: v_span,
                payload,
            });
            if !comma_seen && !self.check(|k| matches!(k, TokenKind::RBrace)) {
                return Err(Diagnostic::new(
                    self.current().span,
                    "expected ',' between enum variants or '}' to close",
                ));
            }
        }
        let close = self.expect_keyword("'}'", |k| matches!(k, TokenKind::RBrace))?;
        Ok(EnumDecl {
            name,
            name_span,
            variants,
            span: start.span.merge(close.span),
        })
    }

    fn parse_struct_decl(&mut self) -> Result<StructDecl, Diagnostic> {
        let start = self.expect_keyword("'struct'", |k| matches!(k, TokenKind::Struct))?;
        let name_tok = self.expect_ident()?;
        let name_span = name_tok.span;
        let name = ident_text(name_tok);
        self.expect_keyword("'{'", |k| matches!(k, TokenKind::LBrace))?;
        let mut fields = Vec::new();
        while !self.check(|k| matches!(k, TokenKind::RBrace | TokenKind::Eof)) {
            let field_name_tok = self.expect_ident()?;
            let field_name_span = field_name_tok.span;
            let field_name = ident_text(field_name_tok);
            self.expect_keyword("':'", |k| matches!(k, TokenKind::Colon))?;
            let ty = self.parse_type()?;
            // Optional trailing comma. Required between
            // fields; allowed before `}`.
            let comma_seen = self
                .match_token(|k| matches!(k, TokenKind::Comma))
                .is_some();
            fields.push(StructField {
                name: field_name,
                ty,
                span: field_name_span,
            });
            if !comma_seen && !self.check(|k| matches!(k, TokenKind::RBrace)) {
                return Err(Diagnostic::new(
                    self.current().span,
                    "expected ',' between struct fields or '}' to close",
                ));
            }
        }
        let close = self.expect_keyword("'}'", |k| matches!(k, TokenKind::RBrace))?;
        Ok(StructDecl {
            name,
            name_span,
            fields,
            span: start.span.merge(close.span),
        })
    }

    /// Parse a `use` declaration. Two forms (closure #245):
    /// - File import: `use "path/to/file.vani";` (quoted
    ///   string, used by the multi-file pipeline).
    /// - Module-path import: `use foo::bar;` (identifier
    ///   followed by `::` and another identifier — brings
    ///   `bar` into scope as an alias for `foo::bar`).
    /// The caller distinguishes by the variant returned.
    fn parse_use(&mut self) -> Result<UseDecl, Diagnostic> {
        let start = self.expect_keyword("'use'", |kind| matches!(kind, TokenKind::Use))?;
        // Peek: string token means file import; identifier
        // means module-path import.
        if matches!(self.current().kind, TokenKind::Str(_)) {
            let path_token = self.expect_string()?;
            let semi = self.expect_keyword("';'", |kind| matches!(kind, TokenKind::Semicolon))?;
            let TokenKind::Str(path) = path_token.kind else {
                unreachable!("expect_string only returns string tokens")
            };
            return Ok(UseDecl::File(Use {
                path,
                span: start.span.merge(semi.span),
            }));
        }
        // Module-path import: `use foo::bar;`.
        let mod_tok = self.expect_ident()?;
        let module = ident_text(mod_tok);
        self.expect_keyword("'::' in `use` path", |k| matches!(k, TokenKind::ColonColon))?;
        let item_tok = self.expect_ident()?;
        let item = ident_text(item_tok);
        let semi = self.expect_keyword("';'", |kind| matches!(kind, TokenKind::Semicolon))?;
        Ok(UseDecl::Path(crate::ast::UsePath {
            module,
            item,
            span: start.span.merge(semi.span),
        }))
    }

    /// Skip tokens until we reach a known top-level start (`fn` / `intent`)
    /// or EOF, so the outer loop can resume parsing the next definition.
    fn sync_to_top_level(&mut self) {
        while !self.check(|kind| {
            matches!(
                kind,
                TokenKind::Fn | TokenKind::Pure | TokenKind::Intent | TokenKind::Use | TokenKind::Eof
            )
        }) {
            self.bump();
        }
    }

    /// Skip tokens until we reach a known statement boundary inside a
    /// function body. Consumes a trailing `;` so the outer loop can resume
    /// cleanly on the next statement.
    fn sync_to_stmt(&mut self) {
        while !self.check(|kind| {
            matches!(
                kind,
                TokenKind::Semicolon
                    | TokenKind::RBrace
                    | TokenKind::Eof
                    | TokenKind::Let
                    | TokenKind::Return
                    | TokenKind::Assert
                    | TokenKind::Prove
                    | TokenKind::Print
                    | TokenKind::If
                    | TokenKind::While
                    | TokenKind::For
                    | TokenKind::Break
                    | TokenKind::Continue
            )
        }) {
            self.bump();
        }
        if self.check(|kind| matches!(kind, TokenKind::Semicolon)) {
            self.bump();
        }
    }

    fn parse_intent(&mut self) -> Result<Intent, Diagnostic> {
        let start = self.expect_keyword("intent", |kind| matches!(kind, TokenKind::Intent))?;
        let text_token = self.expect_string()?;
        let semi = self.expect_keyword("';'", |kind| matches!(kind, TokenKind::Semicolon))?;

        let TokenKind::Str(text) = text_token.kind else {
            unreachable!("expect_string only returns string tokens")
        };

        Ok(Intent {
            text,
            span: start.span.merge(semi.span),
        })
    }

    fn parse_function(&mut self) -> Result<Function, Diagnostic> {
        // Optional `pure` modifier before `fn`.
        let is_pure = self
            .match_token(|kind| matches!(kind, TokenKind::Pure))
            .is_some();
        let fn_token = self.expect_keyword("'fn'", |kind| matches!(kind, TokenKind::Fn))?;
        let name_token = self.expect_ident()?;
        let name_span = name_token.span;
        let name = ident_text(name_token);

        // Optional generic parameter list: `<T1, T2, …>` after
        // the fn name. Names recorded into `type_params`; the
        // checker uses them to recognize `Type::Param(name)`
        // inside the signature / body. T1.4 phase 1: syntax
        // accepted; full monomorphization lands in phase 2.
        let mut type_params: Vec<String> = Vec::new();
        if self.match_token(|k| matches!(k, TokenKind::Less)).is_some() {
            loop {
                let tp_tok = self.expect_ident()?;
                let tp_name = ident_text(tp_tok);
                type_params.push(tp_name);
                if self.match_token(|k| matches!(k, TokenKind::Comma)).is_none() {
                    break;
                }
                // Allow trailing comma before `>` so
                // multi-line generic param lists match
                // the style accepted everywhere else.
                if self.check(|k| matches!(k, TokenKind::Greater | TokenKind::GreaterGreater))
                {
                    break;
                }
            }
            self.expect_close_angle()?;
        }
        // Register the type params so `parse_type` resolves
        // them as `Type::Param` everywhere they appear in
        // this function's signature + body. Cleared at end.
        let saved_tp = self.current_type_params.clone();
        for tp in &type_params {
            self.current_type_params.insert(tp.clone());
        }
        self.expect_keyword("'('", |kind| matches!(kind, TokenKind::LParen))?;
        let mut params = Vec::new();
        if !self.check(|kind| matches!(kind, TokenKind::RParen)) {
            loop {
                let param_name_token = self.expect_ident()?;
                let param_name_span = param_name_token.span;
                let param_name = ident_text(param_name_token);
                self.expect_keyword("':'", |kind| matches!(kind, TokenKind::Colon))?;
                let ty = self.parse_type()?;
                // Until the type-annotation grammar exposes
                // a "type span", the parameter's full span
                // matches its name span. Either is fine for
                // LSP semantic tokens; goto-def will pick
                // the smaller name span when the cursor
                // lands directly on the identifier.
                params.push(Param {
                    name: param_name,
                    ty,
                    name_span: param_name_span,
                    span: param_name_span,
                });

                if self
                    .match_token(|kind| matches!(kind, TokenKind::Comma))
                    .is_none()
                {
                    break;
                }
                // Allow trailing comma in param def so
                // multi-line signatures match the style
                // accepted by struct fields, enum variants,
                // and call-site arg lists.
                if self.check(|k| matches!(k, TokenKind::RParen)) {
                    break;
                }
            }
        }
        self.expect_keyword("')'", |kind| matches!(kind, TokenKind::RParen))?;
        // Unit-return shorthand: `fn name() { body }` (no
        // `->` arrow) is sugar for `fn name() -> i64 { body
        // return 0; }`. The parser auto-fills the i64 return
        // type and the body-rewrite pass appends a synthetic
        // `return 0;` if no explicit return is present. The
        // caller can ignore the i64 (use bare `f();` or
        // `let _ = f();`). T1.0 follow-up (closure #115).
        let unit_return = !self.check(|k| matches!(k, TokenKind::Arrow));
        let return_type = if unit_return {
            Type::I64
        } else {
            self.expect_keyword("'->'", |kind| matches!(kind, TokenKind::Arrow))?;
            self.parse_type()?
        };

        // Optional `where T is Iface, U is Hash, …` bounds.
        // T1.5 phase 1: syntax accepted; checker emits a WIP
        // gate if any program declares interfaces or impls,
        // since dispatch + bounded generics land in phase 2.
        let mut where_clauses: Vec<WhereClause> = Vec::new();
        if self
            .match_token(|k| matches!(k, TokenKind::Where))
            .is_some()
        {
            loop {
                let tp_tok = self.expect_ident()?;
                let tp_span = tp_tok.span;
                let tp_name = ident_text(tp_tok);
                self.expect_keyword("'is'", |k| matches!(k, TokenKind::Is))?;
                let iface_tok = self.expect_ident()?;
                let iface_span = iface_tok.span;
                let iface_name = ident_text(iface_tok);
                where_clauses.push(WhereClause {
                    type_param: tp_name,
                    interface_name: iface_name,
                    span: tp_span.merge(iface_span),
                });
                if self
                    .match_token(|k| matches!(k, TokenKind::Comma))
                    .is_none()
                {
                    break;
                }
                // Allow trailing comma in where-clause
                // bounds list — after the final comma
                // the next token is `{` (body start) or
                // a contract keyword.
                if self.check(|k| {
                    matches!(
                        k,
                        TokenKind::LBrace | TokenKind::Requires | TokenKind::Ensures
                    )
                }) {
                    break;
                }
            }
        }

        let mut requires = Vec::new();
        let mut ensures = Vec::new();
        loop {
            if self
                .match_token(|kind| matches!(kind, TokenKind::Requires))
                .is_some()
            {
                let condition = self.parse_expr()?;
                self.expect_keyword("';'", |kind| matches!(kind, TokenKind::Semicolon))?;
                requires.push(condition);
            } else if self
                .match_token(|kind| matches!(kind, TokenKind::Ensures))
                .is_some()
            {
                let condition = self.parse_expr()?;
                self.expect_keyword("';'", |kind| matches!(kind, TokenKind::Semicolon))?;
                ensures.push(condition);
            } else {
                break;
            }
        }

        self.expect_keyword("'{'", |kind| matches!(kind, TokenKind::LBrace))?;
        let mut body = Vec::new();
        while !self.check(|kind| matches!(kind, TokenKind::RBrace | TokenKind::Eof)) {
            match self.parse_stmt() {
                Ok(s) => body.push(s),
                Err(e) => {
                    self.errors.push(e);
                    self.sync_to_stmt();
                }
            }
        }
        let close = self.expect_keyword("'}'", |kind| matches!(kind, TokenKind::RBrace))?;

        // Unit-return shorthand: append a synthetic `return 0;`
        // if the body didn't end with one. Idempotent — if
        // the user wrote `return 0;` themselves it would
        // already be there.
        if unit_return {
            let last_is_return = matches!(body.last(), Some(Stmt::Return { .. }));
            if !last_is_return {
                body.push(Stmt::Return {
                    expr: Expr {
                        kind: ExprKind::Int(0),
                        span: close.span,
                    },
                    span: close.span,
                });
            }
        }

        self.current_type_params = saved_tp;
        Ok(Function {
            name,
            type_params,
            where_clauses,
            params,
            return_type,
            requires,
            ensures,
            body,
            span: fn_token.span.merge(close.span).merge(name_span),
            is_pure,
        })
    }

    fn parse_type(&mut self) -> Result<Type, Diagnostic> {
        // Tuple type `(T1, T2, …, Tn)` — fixed-size product.
        // Must come before any other `(` consumer in this
        // function. v1 caps at 4 elements; the checker
        // enforces the cap so the parser stays simple.
        // Refines T1.1.
        if matches!(self.current().kind, TokenKind::LParen) {
            let start_span = self.current().span;
            self.bump();
            let mut elements = Vec::new();
            elements.push(self.parse_type()?);
            // Must see at least one comma to qualify as a
            // tuple — a single parenthesized type
            // `(T)` is just grouping.
            self.expect_keyword(
                "',' (tuple type needs at least two elements)",
                |k| matches!(k, TokenKind::Comma),
            )?;
            loop {
                elements.push(self.parse_type()?);
                if self
                    .match_token(|k| matches!(k, TokenKind::Comma))
                    .is_none()
                {
                    break;
                }
                // Trailing comma after last element is allowed.
                if self.check(|k| matches!(k, TokenKind::RParen)) {
                    break;
                }
            }
            self.expect_keyword("')'", |k| matches!(k, TokenKind::RParen))?;
            let _ = start_span;
            return Ok(Type::Tuple(elements));
        }
        // `fn(T1, T2, ...) -> R` — first-class function pointer
        // type. Must come BEFORE the `fn` keyword's primary
        // role as a declaration starter (`fn name() -> R { … }`)
        // would steal the lookahead. Here we're already in a
        // type position, so `fn` unambiguously names the
        // function-pointer type constructor.
        if matches!(self.current().kind, TokenKind::Fn) {
            self.bump();
            self.expect_keyword("'('", |kind| matches!(kind, TokenKind::LParen))?;
            let mut params = Vec::new();
            if !self.check(|kind| matches!(kind, TokenKind::RParen)) {
                loop {
                    params.push(self.parse_type()?);
                    if self
                        .match_token(|kind| matches!(kind, TokenKind::Comma))
                        .is_none()
                    {
                        break;
                    }
                }
            }
            self.expect_keyword("')'", |kind| matches!(kind, TokenKind::RParen))?;
            self.expect_keyword("'->'", |kind| matches!(kind, TokenKind::Arrow))?;
            let ret = self.parse_type()?;
            return Ok(Type::FnPtr(params, Box::new(ret)));
        }
        // Type position borrows: `ref T` / `mut ref T`. Refines
        // T0.0 — replaces the prior `&T` / `&mut T` shape with a
        // keyword form. `mut ref T` is the only valid composition;
        // `ref mut T` is intentionally rejected so the modifier
        // order matches the call-site form (`mut ref x`).
        if self.check(|kind| matches!(kind, TokenKind::Mut))
            && matches!(
                self.tokens.get(self.pos + 1).map(|t| &t.kind),
                Some(TokenKind::Ref)
            )
        {
            self.bump(); // mut
            self.bump(); // ref
            let inner = self.parse_type()?;
            return Ok(Type::RefMut(Box::new(inner)));
        }
        if self
            .match_token(|kind| matches!(kind, TokenKind::Ref))
            .is_some()
        {
            let inner = self.parse_type()?;
            return Ok(Type::Ref(Box::new(inner)));
        }
        // Friendly diagnostic if the source still uses the old
        // `&T` / `&mut T` shape.
        if matches!(self.current().kind, TokenKind::Amp | TokenKind::AndAnd) {
            let span = self.current().span;
            return Err(Diagnostic::new(
                span,
                "use `ref T` / `mut ref T` for reference types (T0.0 syntax sweep)",
            ));
        }

        if self
            .match_token(|kind| matches!(kind, TokenKind::Vec))
            .is_some()
        {
            self.expect_keyword("'<'", |kind| matches!(kind, TokenKind::Less))?;
            let element = self.parse_type()?;
            self.expect_close_angle()?;
            return Ok(Type::Vec(Box::new(element)));
        }

        if self
            .match_token(|kind| matches!(kind, TokenKind::LBracket))
            .is_some()
        {
            let element = self.parse_type()?;
            self.expect_keyword("';'", |kind| matches!(kind, TokenKind::Semicolon))?;
            let length_token = self.bump();
            // Accept either an integer literal or an
            // identifier naming a previously-declared
            // integer-literal const. T0.0 follow-up (closure
            // #120).
            let raw_length = match &length_token.kind {
                TokenKind::Int(v) => *v,
                TokenKind::Ident(name) => match self.const_int_values.get(name) {
                    Some(v) => *v,
                    None => {
                        return Err(Diagnostic::new(
                            length_token.span,
                            format!(
                                "array length '{}' must be a literal integer or a \
                                 previously-declared `const NAME: i64 = <int>;`",
                                name
                            ),
                        ));
                    }
                },
                _ => {
                    return Err(Diagnostic::new(
                        length_token.span,
                        "expected integer literal or const identifier for array length",
                    ));
                }
            };
            if raw_length < 0 {
                return Err(Diagnostic::new(
                    length_token.span,
                    "array length must be non-negative",
                ));
            }
            if raw_length > u64::MAX as i128 {
                return Err(Diagnostic::new(
                    length_token.span,
                    "array length does not fit in u64",
                ));
            }
            self.expect_keyword("']'", |kind| matches!(kind, TokenKind::RBracket))?;
            return Ok(Type::Array {
                element: Box::new(element),
                length: raw_length as u64,
            });
        }

        // `Str` is recognized as a type via the ident token. It's
        // not a lexer keyword because the identifier `Str` may also
        // come up elsewhere; the type position is the only place we
        // accept it for now. `Task` is recognized the same way.
        if let TokenKind::Ident(name) = &self.current().kind {
            // `dyn IfaceName` — fat-pointer interface object.
            // Epic A Phase 1 (closure #220). Contextual keyword
            // recognition keeps the lexer simple; only the type
            // position interprets `dyn` specially.
            if name == "dyn" {
                self.bump();
                let iface_token = self.bump();
                let iface_name = match &iface_token.kind {
                    TokenKind::Ident(n) => n.clone(),
                    _ => {
                        return Err(Diagnostic::new(
                            iface_token.span,
                            "expected an interface name after `dyn`",
                        ));
                    }
                };
                return Ok(Type::Object(iface_name));
            }
            if name == "Str" {
                self.bump();
                return Ok(Type::Str);
            }
            if name == "OwnedStr" {
                self.bump();
                return Ok(Type::OwnedStr);
            }
            if name == "Task" {
                self.bump();
                return Ok(Type::Task);
            }
            if name == "Atomic" {
                self.bump();
                self.expect_keyword("'<'", |kind| matches!(kind, TokenKind::Less))?;
                let element = self.parse_type()?;
                self.expect_close_angle()?;
                return Ok(Type::Atomic(Box::new(element)));
            }
            if name == "Channel" {
                self.bump();
                self.expect_keyword("'<'", |kind| matches!(kind, TokenKind::Less))?;
                let element = self.parse_type()?;
                // Optional `, N` capacity. The checker
                // validates N is a power of two ≥ 1; we just
                // parse the integer literal here.
                let capacity = if self
                    .match_token(|kind| matches!(kind, TokenKind::Comma))
                    .is_some()
                {
                    let tok = self.current().clone();
                    match tok.kind {
                        TokenKind::Int(n) if n > 0 => {
                            self.bump();
                            n as u64
                        }
                        _ => {
                            return Err(Diagnostic::new(
                                tok.span,
                                "expected positive integer capacity after ',' in Channel<T, N>",
                            ));
                        }
                    }
                } else {
                    16
                };
                self.expect_close_angle()?;
                return Ok(Type::Channel(Box::new(element), capacity));
            }
            if name == "Mutex" {
                self.bump();
                self.expect_keyword("'<'", |kind| matches!(kind, TokenKind::Less))?;
                let element = self.parse_type()?;
                self.expect_close_angle()?;
                return Ok(Type::Mutex(Box::new(element)));
            }
            if name == "Guard" {
                self.bump();
                self.expect_keyword("'<'", |kind| matches!(kind, TokenKind::Less))?;
                let element = self.parse_type()?;
                self.expect_close_angle()?;
                return Ok(Type::Guard(Box::new(element)));
            }
            // Single-letter or "T"-prefixed names that match
            // an in-scope type parameter resolve to
            // `Type::Param` so the checker's substitution
            // pass can target them. Anything else (uppercase
            // ident not in `current_type_params`) is a
            // user-declared nominal type — `Type::Struct`
            // is the placeholder until a checker pass
            // determines whether it's actually struct or
            // enum. T1.4.
            if self.current_type_params.contains(name) {
                let n = name.clone();
                self.bump();
                return Ok(Type::Param(n));
            }
            // Closure #242: module-qualified type names. A
            // lowercase-leading ident followed by `::` and an
            // uppercase ident parses as a Type::Struct with
            // the mangled name `<module>__<Type>`. v1 supports
            // a single `::` segment (no nested modules).
            if self
                .tokens
                .get(self.pos + 1)
                .map(|t| matches!(t.kind, TokenKind::ColonColon))
                .unwrap_or(false)
            {
                let mod_name = name.clone();
                self.bump(); // module name
                self.bump(); // ::
                let type_tok = self.expect_ident()?;
                let type_name = ident_text(type_tok);
                let starts_uppercase = type_name
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_uppercase())
                    .unwrap_or(false);
                if !starts_uppercase {
                    return Err(Diagnostic::new(
                        self.current().span,
                        "expected an uppercase type name after `::` \
                         (only types can appear in type position)",
                    ));
                }
                return Ok(Type::Struct(format!("{}__{}", mod_name, type_name)));
            }
            if name
                .chars()
                .next()
                .map(|c| c.is_ascii_uppercase())
                .unwrap_or(false)
            {
                let n = name.clone();
                self.bump();
                return Ok(Type::Struct(n));
            }
        }

        let ty = match self.current().kind {
            TokenKind::I8 => Type::I8,
            TokenKind::I16 => Type::I16,
            TokenKind::I32 => Type::I32,
            TokenKind::I64 => Type::I64,
            TokenKind::U8 => Type::U8,
            TokenKind::U16 => Type::U16,
            TokenKind::U32 => Type::U32,
            TokenKind::U64 => Type::U64,
            TokenKind::F32 => Type::F32,
            TokenKind::F64 => Type::F64,
            TokenKind::Bool => Type::Bool,
            _ => {
                return Err(self.error_here(
                    "expected type like 'i32', 'u64', 'f64', 'bool', 'Str', or '[T; N]'",
                ))
            }
        };
        self.bump();
        Ok(ty)
    }

    fn parse_stmt(&mut self) -> Result<Stmt, Diagnostic> {
        if self.check(|kind| matches!(kind, TokenKind::Let)) {
            self.parse_let_stmt()
        } else if self.check(|kind| matches!(kind, TokenKind::Return)) {
            self.parse_return_stmt()
        } else if self.check(|kind| matches!(kind, TokenKind::Assert)) {
            self.parse_assert_stmt()
        } else if self.check(|kind| matches!(kind, TokenKind::Prove)) {
            self.parse_prove_stmt()
        } else if self.check(|kind| matches!(kind, TokenKind::Print)) {
            self.parse_print_stmt()
        } else if self.check(|kind| matches!(kind, TokenKind::If)) {
            self.parse_if_stmt()
        } else if self.check(|kind| matches!(kind, TokenKind::While)) {
            self.parse_while_stmt()
        } else if self.check(|kind| matches!(kind, TokenKind::For)) {
            self.parse_for_stmt()
        } else if self.check(|kind| matches!(kind, TokenKind::Parallel)) {
            // `parallel for i in start..end { … }` — the modifier
            // precedes `for`. Consume it then dispatch to the
            // for-stmt parser with the parallel flag.
            self.bump();
            self.parse_parallel_for_stmt()
        } else if self.check(|kind| matches!(kind, TokenKind::Task)) {
            self.parse_task_spawn_stmt()
        } else if self.check(|kind| matches!(kind, TokenKind::Join)) {
            self.parse_task_join_stmt()
        } else if self.check(|kind| matches!(kind, TokenKind::Break)) {
            let token = self.bump();
            let semi = self.expect_keyword("';'", |kind| matches!(kind, TokenKind::Semicolon))?;
            Ok(Stmt::Break {
                span: token.span.merge(semi.span),
            })
        } else if self.check(|kind| matches!(kind, TokenKind::Continue)) {
            let token = self.bump();
            let semi = self.expect_keyword("';'", |kind| matches!(kind, TokenKind::Semicolon))?;
            Ok(Stmt::Continue {
                span: token.span.merge(semi.span),
            })
        } else if self.looks_like_assignment() {
            self.parse_assign_stmt()
        } else if self.looks_like_index_assign() {
            self.parse_index_assign_stmt()
        } else if self.looks_like_field_assign() {
            self.parse_field_assign_stmt()
        } else if self.looks_like_index_then_field_assign() {
            // Parse `<ident>[…].field = …;` directly into
            // `Stmt::IndexAssign` with a non-empty
            // `field_path`. T1.2 phase 2b follow-up.
            self.parse_index_then_field_assign_stmt()
        } else if self.check(|kind| matches!(kind, TokenKind::LBrace)) {
            // Bare block `{ … }` as a statement — provides an
            // explicit nested scope. Desugars to
            // `if true { … }` at parse time so the existing
            // If-scope machinery handles binding visibility,
            // affine moves, and codegen. The constant-fold
            // path collapses the `if true` away in both
            // backends. T1.0 follow-up (closure #116).
            let start = self.current().span;
            let stmts = self.parse_block()?;
            let end_span = stmts.last().map(|s| s.span()).unwrap_or(start);
            Ok(Stmt::If {
                cond: Expr {
                    kind: ExprKind::Bool(true),
                    span: start,
                },
                then_body: stmts,
                else_body: Vec::new(),
                span: start.merge(end_span),
            })
        } else {
            // Last-chance fallback: try to parse an expression
            // followed by `;`. This enables side-effect-bearing
            // call / method-call statements (`x.bump();`, `foo();`)
            // without forcing users to write `let _ = …;`. The
            // expression's value is discarded; the checker enforces
            // that the result isn't an affine type that would silently
            // leak (and the existing `let _ = …` desugaring covers
            // the drop chain for Copy results).
            let saved_pos = self.pos;
            let start_span = self.current().span;
            match self.parse_expr() {
                Ok(expr) => {
                    if let Some(semi) = self.match_token(|k| matches!(k, TokenKind::Semicolon)) {
                        // Restrict to call-shaped expressions so we
                        // don't accidentally absorb things that look
                        // like statements gone wrong (e.g. `x;`).
                        if !matches!(expr.kind, ExprKind::Call { .. } | ExprKind::MethodCall { .. }) {
                            self.pos = saved_pos;
                            return Err(self.error_here("expected statement"));
                        }
                        return Ok(Stmt::Let {
                            name: "_".to_string(),
                            annotation: None,
                            expr,
                            span: start_span.merge(semi.span),
                        });
                    }
                    self.pos = saved_pos;
                    Err(self.error_here("expected statement"))
                }
                Err(_) => {
                    self.pos = saved_pos;
                    Err(self.error_here("expected statement"))
                }
            }
        }
    }

    /// `<ident> [ … ] . <ident> =` (or longer chain) —
    /// the not-yet-supported mixed-place-assign shape.
    /// Used to give users a clean diagnostic + workaround
    /// instead of the opaque "expected statement". v1
    /// limitation; lifting it would require place-tracker
    /// codegen for chained-index-and-field lvalues.
    fn looks_like_index_then_field_assign(&self) -> bool {
        if !matches!(self.current().kind, TokenKind::Ident(_)) {
            return false;
        }
        let mut i = self.pos + 1;
        if !matches!(self.tokens.get(i).map(|t| &t.kind), Some(TokenKind::LBracket)) {
            return false;
        }
        let mut depth: i32 = 1;
        i += 1;
        while let Some(tok) = self.tokens.get(i) {
            match tok.kind {
                TokenKind::LBracket => depth += 1,
                TokenKind::RBracket => {
                    depth -= 1;
                    if depth == 0 {
                        i += 1;
                        break;
                    }
                }
                TokenKind::Eof => return false,
                _ => {}
            }
            i += 1;
        }
        // After `]`, look for `.<ident>` followed eventually
        // by `=`.
        if !matches!(self.tokens.get(i).map(|t| &t.kind), Some(TokenKind::Dot)) {
            return false;
        }
        i += 1;
        // Scan past `.<ident>(.<ident>)*` to find the `=`.
        loop {
            if !matches!(
                self.tokens.get(i).map(|t| &t.kind),
                Some(TokenKind::Ident(_))
            ) {
                return false;
            }
            i += 1;
            match self.tokens.get(i).map(|t| &t.kind) {
                Some(TokenKind::Equal) => return true,
                Some(TokenKind::Dot) => {
                    i += 1;
                }
                _ => return false,
            }
        }
    }

    fn looks_like_assignment(&self) -> bool {
        if !matches!(self.current().kind, TokenKind::Ident(_)) {
            return false;
        }
        matches!(
            self.tokens.get(self.pos + 1).map(|t| &t.kind),
            Some(TokenKind::Equal)
        )
    }

    /// `<ident> (. <ident>)+ =` — a chain of field accesses
    /// followed by an `=`. Used to disambiguate
    /// `p.x = expr;` (field assignment) from a method call.
    /// The chain must end with an ident (not an integer
    /// tuple-index — tuple slots aren't reassignable in v1).
    /// T1.2 phase 2a follow-up.
    fn looks_like_field_assign(&self) -> bool {
        if !matches!(self.current().kind, TokenKind::Ident(_)) {
            return false;
        }
        let mut i = self.pos + 1;
        let mut saw_dot = false;
        loop {
            match self.tokens.get(i).map(|t| &t.kind) {
                Some(TokenKind::Dot) => {
                    saw_dot = true;
                    i += 1;
                    // Next must be an ident (field name)
                    // and not be followed by `(` (that would
                    // be a method call, not a place).
                    if !matches!(
                        self.tokens.get(i).map(|t| &t.kind),
                        Some(TokenKind::Ident(_))
                    ) {
                        return false;
                    }
                    if matches!(
                        self.tokens.get(i + 1).map(|t| &t.kind),
                        Some(TokenKind::LParen)
                    ) {
                        return false;
                    }
                    i += 1;
                }
                Some(TokenKind::Equal) if saw_dot => return true,
                _ => return false,
            }
        }
    }

    fn parse_field_assign_stmt(&mut self) -> Result<Stmt, Diagnostic> {
        // Parse the LHS place expression as a chain of
        // `<ident>(.<ident>)+`. The last `.<ident>` becomes
        // the FieldAssign's field; everything before is the
        // object expression.
        let head_tok = self.expect_ident()?;
        let head_span = head_tok.span;
        let head_name = ident_text(head_tok);
        let mut object = Expr {
            kind: ExprKind::Var(head_name),
            span: head_span,
        };
        // Collect all but the last `.ident` into nested
        // FieldAccess.
        loop {
            // The lookahead above guaranteed at least one
            // `. ident` here.
            self.expect_keyword("'.'", |k| matches!(k, TokenKind::Dot))?;
            let field_tok = self.expect_ident()?;
            let field_span = field_tok.span;
            let field_name = ident_text(field_tok);
            // Is this the final `.field` before `=`? If
            // yes, stop here and emit FieldAssign. Else
            // keep wrapping FieldAccess.
            if matches!(self.current().kind, TokenKind::Equal) {
                self.expect_keyword("'='", |k| matches!(k, TokenKind::Equal))?;
                let value = self.parse_expr()?;
                let semi = self
                    .expect_keyword("';'", |k| matches!(k, TokenKind::Semicolon))?;
                return Ok(Stmt::FieldAssign {
                    object,
                    field: field_name,
                    field_span,
                    value,
                    span: head_span.merge(semi.span),
                });
            }
            object = Expr {
                kind: ExprKind::FieldAccess {
                    object: Box::new(object),
                    field: field_name,
                },
                span: head_span.merge(field_span),
            };
        }
    }

    fn looks_like_index_assign(&self) -> bool {
        if !matches!(self.current().kind, TokenKind::Ident(_)) {
            return false;
        }
        // Scan past a single `[ ... ]` (matching brackets) and check for `=`.
        let mut i = self.pos + 1;
        if !matches!(self.tokens.get(i).map(|t| &t.kind), Some(TokenKind::LBracket)) {
            return false;
        }
        let mut depth: i32 = 1;
        i += 1;
        while let Some(tok) = self.tokens.get(i) {
            match tok.kind {
                TokenKind::LBracket => depth += 1,
                TokenKind::RBracket => {
                    depth -= 1;
                    if depth == 0 {
                        i += 1;
                        break;
                    }
                }
                TokenKind::Eof => return false,
                _ => {}
            }
            i += 1;
        }
        matches!(self.tokens.get(i).map(|t| &t.kind), Some(TokenKind::Equal))
    }

    fn parse_index_assign_stmt(&mut self) -> Result<Stmt, Diagnostic> {
        let name_token = self.expect_ident()?;
        let name_span = name_token.span;
        let name = ident_text(name_token);
        self.expect_keyword("'['", |kind| matches!(kind, TokenKind::LBracket))?;
        let index = self.parse_expr()?;
        self.expect_keyword("']'", |kind| matches!(kind, TokenKind::RBracket))?;
        self.expect_keyword("'='", |kind| matches!(kind, TokenKind::Equal))?;
        let value = self.parse_expr()?;
        let semi = self.expect_keyword("';'", |kind| matches!(kind, TokenKind::Semicolon))?;
        Ok(Stmt::IndexAssign {
            name,
            index,
            field_path: Vec::new(),
            value,
            span: name_span.merge(semi.span),
        })
    }

    /// Parse `<ident>[<index>].<field>(.<field>)* = <expr>;`
    /// into `Stmt::IndexAssign` with a non-empty `field_path`.
    /// The lookahead in `looks_like_index_then_field_assign`
    /// has already validated the surface shape; this just
    /// rebuilds the AST nodes. T1.2 phase 2b follow-up.
    fn parse_index_then_field_assign_stmt(&mut self) -> Result<Stmt, Diagnostic> {
        let name_token = self.expect_ident()?;
        let name_span = name_token.span;
        let name = ident_text(name_token);
        self.expect_keyword("'['", |kind| matches!(kind, TokenKind::LBracket))?;
        let index = self.parse_expr()?;
        self.expect_keyword("']'", |kind| matches!(kind, TokenKind::RBracket))?;
        // Parse one-or-more `.<field>` segments.
        let mut field_path: Vec<String> = Vec::new();
        while self
            .match_token(|k| matches!(k, TokenKind::Dot))
            .is_some()
        {
            let field_tok = self.expect_ident()?;
            field_path.push(ident_text(field_tok));
            if !matches!(
                self.current().kind,
                TokenKind::Dot | TokenKind::Equal
            ) {
                return Err(self.error_here(
                    "expected '.<field>' or '=' after indexed field-access",
                ));
            }
        }
        self.expect_keyword("'='", |kind| matches!(kind, TokenKind::Equal))?;
        let value = self.parse_expr()?;
        let semi = self.expect_keyword("';'", |kind| matches!(kind, TokenKind::Semicolon))?;
        Ok(Stmt::IndexAssign {
            name,
            index,
            field_path,
            value,
            span: name_span.merge(semi.span),
        })
    }

    fn parse_assign_stmt(&mut self) -> Result<Stmt, Diagnostic> {
        let name_token = self.expect_ident()?;
        let name_span = name_token.span;
        let name = ident_text(name_token);
        self.expect_keyword("'='", |kind| matches!(kind, TokenKind::Equal))?;
        let expr = self.parse_expr()?;
        let semi = self.expect_keyword("';'", |kind| matches!(kind, TokenKind::Semicolon))?;
        Ok(Stmt::Assign {
            name,
            expr,
            span: name_span.merge(semi.span),
        })
    }

    fn parse_if_stmt(&mut self) -> Result<Stmt, Diagnostic> {
        let start = self.expect_keyword("'if'", |kind| matches!(kind, TokenKind::If))?;
        let cond = self.parse_expr()?;
        let then_body = self.parse_block()?;
        let (else_body, end_span) = if self
            .match_token(|kind| matches!(kind, TokenKind::Else))
            .is_some()
        {
            if self.check(|kind| matches!(kind, TokenKind::If)) {
                // else-if: re-parse as a nested if statement inside a one-statement else block.
                let inner = self.parse_if_stmt()?;
                let span = inner.span();
                (vec![inner], span)
            } else {
                let stmts = self.parse_block()?;
                let span = stmts
                    .last()
                    .map(|s| s.span())
                    .unwrap_or(start.span);
                (stmts, span)
            }
        } else {
            (Vec::new(), then_body.last().map(|s| s.span()).unwrap_or(start.span))
        };
        Ok(Stmt::If {
            cond,
            then_body,
            else_body,
            span: start.span.merge(end_span),
        })
    }

    fn parse_for_stmt(&mut self) -> Result<Stmt, Diagnostic> {
        self.parse_for_stmt_inner(false)
    }

    fn parse_task_spawn_stmt(&mut self) -> Result<Stmt, Diagnostic> {
        let task_tok = self.expect_keyword("'task'", |kind| matches!(kind, TokenKind::Task))?;
        let name_tok = self.expect_ident()?;
        let name = ident_text(name_tok);
        self.expect_keyword("'{'", |kind| matches!(kind, TokenKind::LBrace))?;
        let mut body = Vec::new();
        while !self.check(|kind| matches!(kind, TokenKind::RBrace | TokenKind::Eof)) {
            body.push(self.parse_stmt()?);
        }
        let close = self.expect_keyword("'}'", |kind| matches!(kind, TokenKind::RBrace))?;
        Ok(Stmt::TaskSpawn {
            name,
            body,
            span: task_tok.span.merge(close.span),
        })
    }

    fn parse_task_join_stmt(&mut self) -> Result<Stmt, Diagnostic> {
        let join_tok = self.expect_keyword("'join'", |kind| matches!(kind, TokenKind::Join))?;
        let name_tok = self.expect_ident()?;
        let name = ident_text(name_tok);
        let semi = self.expect_keyword("';'", |kind| matches!(kind, TokenKind::Semicolon))?;
        Ok(Stmt::TaskJoin {
            name,
            span: join_tok.span.merge(semi.span),
        })
    }

    fn parse_parallel_for_stmt(&mut self) -> Result<Stmt, Diagnostic> {
        // `parallel` was just bumped by the caller. Only the range
        // form supports the parallel marker — iter-style `for x in
        // xs` consumes the collection (which can't be raced over).
        self.parse_for_stmt_inner(true)
    }

    fn parse_for_stmt_inner(&mut self, parallel: bool) -> Result<Stmt, Diagnostic> {
        let start_tok =
            self.expect_keyword("'for'", |kind| matches!(kind, TokenKind::For))?;
        let var_tok = self.expect_ident()?;
        let var = ident_text(var_tok);
        // The two `for` shapes are now disambiguated by the
        // post-counter keyword:
        //   `for VAR in EXPR { ... }`           → collection-iter
        //                                          (consuming or
        //                                           borrowing — `ref EXPR`)
        //   `for VAR from LO to HI { ... }`     → range form
        // Refines T0.0. The prior `0..n` range shape is gone.
        if self.match_token(|k| matches!(k, TokenKind::In)).is_some() {
            // Borrowing form: `for x in ref xs { ... }`. The old
            // `for x in &xs { ... }` shape is gone — surface a
            // friendly hint if encountered.
            if matches!(self.current().kind, TokenKind::Amp) {
                let span = self.current().span;
                return Err(Diagnostic::new(
                    span,
                    "use `for VAR in ref XS { … }` to iterate by borrow (T0.0)",
                ));
            }
            let consumes = !self
                .match_token(|k| matches!(k, TokenKind::Ref))
                .is_some();
            if parallel {
                return Err(Diagnostic::new(
                    start_tok.span,
                    "'parallel' is only valid on a range-form for loop",
                ));
            }
            let collection_tok = self.expect_ident()?;
            let collection = ident_text(collection_tok);
            let body = self.parse_block()?;
            let end_span = body
                .last()
                .map(|s| s.span())
                .unwrap_or(start_tok.span);
            return Ok(Stmt::ForIter {
                var,
                collection,
                consumes,
                body,
                span: start_tok.span.merge(end_span),
            });
        }
        // Range form: `for i from LO to HI invariant ...; { body }`.
        // The lower bound expression follows `from`, the upper
        // follows `to`. Refines T0.0; was `for i in LO..HI`.
        self.expect_keyword(
            "'from' (range form) or 'in' (collection-iter)",
            |k| matches!(k, TokenKind::From),
        )?;
        let start = self.parse_expr()?;
        self.expect_keyword("'to'", |k| matches!(k, TokenKind::To))?;
        let end = self.parse_expr()?;
        let invariants = self.parse_invariants()?;
        let reductions = self.parse_reductions()?;
        if !reductions.is_empty() && !parallel {
            return Err(Diagnostic::new(
                reductions[0].span,
                "'reduce' clauses are only valid on a `parallel for` loop",
            ));
        }
        let body = self.parse_block()?;
        let end_span = body.last().map(|s| s.span()).unwrap_or(start_tok.span);
        Ok(Stmt::For {
            var,
            start,
            end,
            invariants,
            body,
            span: start_tok.span.merge(end_span),
            parallel,
            reductions,
        })
    }

    fn parse_reductions(&mut self) -> Result<Vec<Reduction>, Diagnostic> {
        let mut out = Vec::new();
        while let Some(start) = self.match_token(|kind| matches!(kind, TokenKind::Reduce)) {
            let var_tok = self.expect_ident()?;
            let var = ident_text(var_tok);
            self.expect_keyword("'with'", |kind| matches!(kind, TokenKind::With))?;
            // Reduction operator — currently `+` only. Other
            // associative ops (`*`, min, max) are an easy follow-on
            // once we have richer operator-symbol parsing for
            // non-Binary positions.
            let op_tok = self.current().clone();
            let op = match op_tok.kind {
                TokenKind::Plus => {
                    self.bump();
                    ReductionOp::Add
                }
                TokenKind::Star => {
                    self.bump();
                    ReductionOp::Mul
                }
                TokenKind::AndAnd => {
                    self.bump();
                    ReductionOp::And
                }
                TokenKind::OrOr => {
                    self.bump();
                    ReductionOp::Or
                }
                TokenKind::Amp => {
                    self.bump();
                    ReductionOp::BitAnd
                }
                TokenKind::Pipe => {
                    self.bump();
                    ReductionOp::BitOr
                }
                TokenKind::Caret => {
                    self.bump();
                    ReductionOp::BitXor
                }
                // `min` and `max` are context-sensitive
                // identifiers (not reserved keywords) — match
                // them by literal text so users can declare
                // struct fields / locals with those names
                // outside this clause.
                TokenKind::Ident(ref n) if n == "min" => {
                    self.bump();
                    ReductionOp::Min
                }
                TokenKind::Ident(ref n) if n == "max" => {
                    self.bump();
                    ReductionOp::Max
                }
                _ => {
                    return Err(Diagnostic::new(
                        op_tok.span,
                        "expected reduction operator (one of `+`, `*`, `&&`, `||`, `&`, `|`, `^`, `min`, `max`)",
                    ));
                }
            };
            let semi = self.expect_keyword("';'", |kind| matches!(kind, TokenKind::Semicolon))?;
            out.push(Reduction {
                var,
                op,
                span: start.span.merge(semi.span),
            });
        }
        Ok(out)
    }

    fn parse_invariants(&mut self) -> Result<Vec<Expr>, Diagnostic> {
        let mut invariants = Vec::new();
        while self
            .match_token(|kind| matches!(kind, TokenKind::Invariant))
            .is_some()
        {
            let expr = self.parse_expr()?;
            self.expect_keyword("';'", |kind| matches!(kind, TokenKind::Semicolon))?;
            invariants.push(expr);
        }
        Ok(invariants)
    }

    fn parse_while_stmt(&mut self) -> Result<Stmt, Diagnostic> {
        let start = self.expect_keyword("'while'", |kind| matches!(kind, TokenKind::While))?;
        let cond = self.parse_expr()?;
        let invariants = self.parse_invariants()?;
        let body = self.parse_block()?;
        let end_span = body.last().map(|s| s.span()).unwrap_or(start.span);
        Ok(Stmt::While {
            cond,
            invariants,
            body,
            span: start.span.merge(end_span),
        })
    }

    fn parse_block(&mut self) -> Result<Vec<Stmt>, Diagnostic> {
        self.expect_keyword("'{'", |kind| matches!(kind, TokenKind::LBrace))?;
        let mut body = Vec::new();
        while !self.check(|kind| matches!(kind, TokenKind::RBrace | TokenKind::Eof)) {
            match self.parse_stmt() {
                Ok(s) => body.push(s),
                Err(e) => {
                    self.errors.push(e);
                    self.sync_to_stmt();
                }
            }
        }
        self.expect_keyword("'}'", |kind| matches!(kind, TokenKind::RBrace))?;
        Ok(body)
    }

    fn parse_let_stmt(&mut self) -> Result<Stmt, Diagnostic> {
        let start = self.expect_keyword("'let'", |kind| matches!(kind, TokenKind::Let))?;
        // Destructure form: `let (a, b, …) = expr;` —
        // produces `Stmt::LetTuple`. The checker desugars to
        // a sequence of `Let`s under the hood. T1.1.
        if self.check(|k| matches!(k, TokenKind::LParen)) {
            self.bump();
            let mut names = Vec::new();
            loop {
                let tok = self.expect_ident()?;
                names.push(ident_text(tok));
                if self
                    .match_token(|k| matches!(k, TokenKind::Comma))
                    .is_none()
                {
                    break;
                }
                if self.check(|k| matches!(k, TokenKind::RParen)) {
                    break;
                }
            }
            self.expect_keyword("')'", |k| matches!(k, TokenKind::RParen))?;
            let annotation = if self
                .match_token(|k| matches!(k, TokenKind::Colon))
                .is_some()
            {
                Some(self.parse_type()?)
            } else {
                None
            };
            self.expect_keyword("'='", |k| matches!(k, TokenKind::Equal))?;
            let expr = self.parse_expr()?;
            let semi = self.expect_keyword("';'", |k| matches!(k, TokenKind::Semicolon))?;
            if names.len() < 2 {
                return Err(Diagnostic::new(
                    start.span.merge(semi.span),
                    "destructure-let needs at least two names; use plain `let` for single bindings",
                ));
            }
            return Ok(Stmt::LetTuple {
                names,
                annotation,
                expr,
                span: start.span.merge(semi.span),
            });
        }
        let name_token = self.expect_ident()?;
        let name = ident_text(name_token);
        let annotation = if self
            .match_token(|kind| matches!(kind, TokenKind::Colon))
            .is_some()
        {
            Some(self.parse_type()?)
        } else {
            None
        };
        self.expect_keyword("'='", |kind| matches!(kind, TokenKind::Equal))?;
        let expr = self.parse_expr()?;
        let semi = self.expect_keyword("';'", |kind| matches!(kind, TokenKind::Semicolon))?;

        Ok(Stmt::Let {
            name,
            annotation,
            expr,
            span: start.span.merge(semi.span),
        })
    }

    fn parse_return_stmt(&mut self) -> Result<Stmt, Diagnostic> {
        let start = self.expect_keyword("'return'", |kind| matches!(kind, TokenKind::Return))?;
        let expr = self.parse_expr()?;
        let semi = self.expect_keyword("';'", |kind| matches!(kind, TokenKind::Semicolon))?;
        Ok(Stmt::Return {
            expr,
            span: start.span.merge(semi.span),
        })
    }

    fn parse_assert_stmt(&mut self) -> Result<Stmt, Diagnostic> {
        let start = self.expect_keyword("'assert'", |kind| matches!(kind, TokenKind::Assert))?;
        let expr = self.parse_expr()?;
        // Optional `, "message"` between the condition and the semicolon.
        let message = if self
            .match_token(|kind| matches!(kind, TokenKind::Comma))
            .is_some()
        {
            let msg_token = self.expect_string()?;
            let TokenKind::Str(s) = msg_token.kind else {
                unreachable!("expect_string only returns Str tokens")
            };
            Some(s)
        } else {
            None
        };
        let semi = self.expect_keyword("';'", |kind| matches!(kind, TokenKind::Semicolon))?;
        Ok(Stmt::Assert {
            expr,
            message,
            span: start.span.merge(semi.span),
        })
    }

    fn parse_prove_stmt(&mut self) -> Result<Stmt, Diagnostic> {
        let start = self.expect_keyword("'prove'", |kind| matches!(kind, TokenKind::Prove))?;
        let expr = self.parse_expr()?;
        let semi = self.expect_keyword("';'", |kind| matches!(kind, TokenKind::Semicolon))?;
        Ok(Stmt::Prove {
            expr,
            span: start.span.merge(semi.span),
        })
    }

    fn parse_print_stmt(&mut self) -> Result<Stmt, Diagnostic> {
        let start = self.expect_keyword("'print'", |kind| matches!(kind, TokenKind::Print))?;
        // Comma-separated items: each is a string literal or an
        // expression. `print "x =", x, "(done)";` is legal.
        let mut items = Vec::new();
        loop {
            items.push(self.parse_print_item()?);
            if self
                .match_token(|kind| matches!(kind, TokenKind::Comma))
                .is_some()
            {
                continue;
            }
            break;
        }
        let semi = self.expect_keyword("';'", |kind| matches!(kind, TokenKind::Semicolon))?;
        Ok(Stmt::Print {
            items,
            span: start.span.merge(semi.span),
        })
    }

    fn parse_print_item(&mut self) -> Result<crate::ast::PrintItem, Diagnostic> {
        use crate::ast::PrintItem;
        if let TokenKind::Str(_) = &self.current().kind {
            let tok = self.bump();
            match tok.kind {
                TokenKind::Str(s) => Ok(PrintItem::Str(s)),
                _ => unreachable!(),
            }
        } else {
            let expr = self.parse_expr()?;
            Ok(PrintItem::Expr(expr))
        }
    }

    fn parse_expr(&mut self) -> Result<Expr, Diagnostic> {
        self.parse_binary_expr(1)
    }

    fn parse_binary_expr(&mut self, min_precedence: u8) -> Result<Expr, Diagnostic> {
        let mut left = self.parse_unary_expr()?;

        while let Some((op, precedence)) = self.current_binary_op() {
            if precedence < min_precedence {
                break;
            }

            self.bump();
            let right = self.parse_binary_expr(precedence + 1)?;
            let span = left.span.merge(right.span);
            left = Expr {
                kind: ExprKind::Binary {
                    op,
                    left: Box::new(left),
                    right: Box::new(right),
                },
                span,
            };
        }

        Ok(left)
    }

    fn parse_unary_expr(&mut self) -> Result<Expr, Diagnostic> {
        // Borrow expressions: `ref x` (immutable) and
        // `mut ref x` (mutable). The old `&x` / `&mut x`
        // prefix is gone — a friendly diagnostic points at
        // the new shape. Refines T0.0.
        let mut_then_ref = self.check(|k| matches!(k, TokenKind::Mut))
            && matches!(
                self.tokens.get(self.pos + 1).map(|t| &t.kind),
                Some(TokenKind::Ref)
            );
        if mut_then_ref {
            let mut_tok = self.bump();
            self.bump(); // ref
            let inner = self.parse_unary_expr()?;
            let span = mut_tok.span.merge(inner.span);
            return Ok(Expr {
                kind: ExprKind::RefMut {
                    inner: Box::new(inner),
                },
                span,
            });
        }
        if let Some(token) = self.match_token(|kind| matches!(kind, TokenKind::Ref)) {
            let inner = self.parse_unary_expr()?;
            let span = token.span.merge(inner.span);
            return Ok(Expr {
                kind: ExprKind::Ref {
                    inner: Box::new(inner),
                },
                span,
            });
        }
        // Old `&` prefix borrow — surface a guidance error.
        // We can't accept it here because the parser still
        // needs `&` available as the bitwise-AND binary op.
        if let Some(token) = self.match_token(|kind| matches!(kind, TokenKind::Amp)) {
            return Err(Diagnostic::new(
                token.span,
                "use `ref x` (or `mut ref x`) instead of `&x` for borrows (T0.0)",
            ));
        }
        if let Some(token) = self.match_token(|kind| matches!(kind, TokenKind::Minus)) {
            let expr = self.parse_unary_expr()?;
            let span = token.span.merge(expr.span);
            Ok(Expr {
                kind: ExprKind::Unary {
                    op: UnaryOp::Neg,
                    expr: Box::new(expr),
                },
                span,
            })
        } else if let Some(token) = self.match_token(|kind| matches!(kind, TokenKind::Bang)) {
            let expr = self.parse_unary_expr()?;
            let span = token.span.merge(expr.span);
            Ok(Expr {
                kind: ExprKind::Unary {
                    op: UnaryOp::Not,
                    expr: Box::new(expr),
                },
                span,
            })
        } else {
            self.parse_call_expr()
        }
    }

    fn parse_call_expr(&mut self) -> Result<Expr, Diagnostic> {
        let mut expr = self.parse_primary_expr()?;

        loop {
            if self
                .match_token(|kind| matches!(kind, TokenKind::LParen))
                .is_some()
            {
                // Preserve the callee identifier's span
                // before we move `expr.kind` into the Var
                // destructure below — `expr.span` is the
                // Var span (just the identifier) because
                // the primary parser wraps Var in an Expr
                // with its span set to the Ident's span.
                let name_span = expr.span;
                let ExprKind::Var(name) = expr.kind else {
                    return Err(Diagnostic::new(
                        expr.span,
                        "only named functions can be called",
                    ));
                };

                let mut args = Vec::new();
                if !self.check(|kind| matches!(kind, TokenKind::RParen)) {
                    loop {
                        args.push(self.parse_expr()?);
                        if self
                            .match_token(|kind| matches!(kind, TokenKind::Comma))
                            .is_none()
                        {
                            break;
                        }
                        // Allow trailing comma before `)` so
                        // multi-line call sites can use the
                        // same style as struct/enum/array
                        // literals.
                        if self.check(|k| matches!(k, TokenKind::RParen)) {
                            break;
                        }
                    }
                }
                let close = self.expect_keyword("')'", |kind| matches!(kind, TokenKind::RParen))?;
                let span = name_span.merge(close.span);
                expr = Expr {
                    kind: ExprKind::Call { name, name_span, args },
                    span,
                };
            } else if self
                .match_token(|kind| matches!(kind, TokenKind::As))
                .is_some()
            {
                let ty = self.parse_type()?;
                expr = Expr {
                    span: expr.span,
                    kind: ExprKind::Cast {
                        expr: Box::new(expr),
                        ty,
                    },
                };
            } else if self
                .match_token(|kind| matches!(kind, TokenKind::LBracket))
                .is_some()
            {
                let index = self.parse_expr()?;
                let close =
                    self.expect_keyword("']'", |kind| matches!(kind, TokenKind::RBracket))?;
                let span = expr.span.merge(close.span);
                expr = Expr {
                    kind: ExprKind::Index {
                        array: Box::new(expr),
                        index: Box::new(index),
                    },
                    span,
                };
            } else if self
                .match_token(|k| matches!(k, TokenKind::Dot))
                .is_some()
            {
                // `expr.<index>` (tuple access) or
                // `expr.<ident>` (struct field). Disambiguate
                // on the next token. T1.1 / T1.2.
                let next = self.bump();
                let span = expr.span.merge(next.span);
                match next.kind {
                    TokenKind::Int(value) => {
                        if value < 0 || value > u32::MAX as i128 {
                            return Err(Diagnostic::new(
                                next.span,
                                "tuple index must fit in u32",
                            ));
                        }
                        expr = Expr {
                            kind: ExprKind::TupleAccess {
                                tuple: Box::new(expr),
                                index: value as u32,
                            },
                            span,
                        };
                    }
                    // `t.0.0` lexes as `t`, `.`, `Float(0.0)`
                    // because the lexer greedily consumes
                    // `0.0` as a numeric literal. When both
                    // halves are non-negative integers and
                    // the string form is a single dot
                    // separator, treat it as nested tuple
                    // access. T1.1 + nested-tuple support.
                    TokenKind::Float(value) => {
                        // `{:?}` gives round-trippable form
                        // like `0.0`, while `{}` strips
                        // trailing-zero fractions to `0`.
                        let s = format!("{:?}", value);
                        let mut parts = s.split('.');
                        let n_str = parts.next();
                        let m_str = parts.next();
                        let extra = parts.next();
                        match (n_str, m_str, extra) {
                            (Some(n_str), Some(m_str), None) => {
                                if let (Ok(n), Ok(m)) = (
                                    n_str.parse::<u32>(),
                                    m_str.parse::<u32>(),
                                ) {
                                    let inner_span = next.span;
                                    expr = Expr {
                                        kind: ExprKind::TupleAccess {
                                            tuple: Box::new(expr),
                                            index: n,
                                        },
                                        span,
                                    };
                                    expr = Expr {
                                        kind: ExprKind::TupleAccess {
                                            tuple: Box::new(expr),
                                            index: m,
                                        },
                                        span: span.merge(inner_span),
                                    };
                                } else {
                                    return Err(Diagnostic::new(
                                        next.span,
                                        "expected integer (tuple index) or \
                                         identifier (field name) after '.'",
                                    ));
                                }
                            }
                            _ => {
                                return Err(Diagnostic::new(
                                    next.span,
                                    "expected integer (tuple index) or \
                                     identifier (field name) after '.'",
                                ));
                            }
                        }
                    }
                    TokenKind::Ident(name_text) => {
                        // Disambiguate: `expr.foo(args)` is a
                        // MethodCall; `expr.foo` is a
                        // FieldAccess. T1.2 phase 2a.
                        if self.check(|k| matches!(k, TokenKind::LParen)) {
                            let method_span = next.span;
                            self.expect_keyword("'('", |k| matches!(k, TokenKind::LParen))?;
                            let mut args = Vec::new();
                            if !self.check(|k| matches!(k, TokenKind::RParen)) {
                                loop {
                                    args.push(self.parse_expr()?);
                                    if self
                                        .match_token(|k| matches!(k, TokenKind::Comma))
                                        .is_none()
                                    {
                                        break;
                                    }
                                    if self.check(|k| matches!(k, TokenKind::RParen)) {
                                        break;
                                    }
                                }
                            }
                            let close = self
                                .expect_keyword("')'", |k| matches!(k, TokenKind::RParen))?;
                            expr = Expr {
                                kind: ExprKind::MethodCall {
                                    receiver: Box::new(expr),
                                    method: name_text,
                                    method_span,
                                    args,
                                },
                                span: span.merge(close.span),
                            };
                        } else {
                            expr = Expr {
                                kind: ExprKind::FieldAccess {
                                    object: Box::new(expr),
                                    field: name_text,
                                },
                                span,
                            };
                        }
                    }
                    _ => {
                        return Err(Diagnostic::new(
                            next.span,
                            "expected integer (tuple index) or identifier (field name) after '.'",
                        ));
                    }
                }
            } else {
                return Ok(expr);
            }
        }
    }

    fn parse_primary_expr(&mut self) -> Result<Expr, Diagnostic> {
        let token = self.bump();
        match token.kind {
            TokenKind::Int(value) => Ok(Expr {
                kind: ExprKind::Int(value),
                span: token.span,
            }),
            TokenKind::Float(value) => Ok(Expr {
                kind: ExprKind::Float(value),
                span: token.span,
            }),
            TokenKind::True => Ok(Expr {
                kind: ExprKind::Bool(true),
                span: token.span,
            }),
            TokenKind::False => Ok(Expr {
                kind: ExprKind::Bool(false),
                span: token.span,
            }),
            TokenKind::Str(text) => Ok(Expr {
                kind: ExprKind::Str(text),
                span: token.span,
            }),
            TokenKind::LBrace => {
                // Block expression: `{ stmt; stmt; tail-expr }`.
                // V1 admits `let` bindings and `print` stmts
                // in the prefix; the checker enforces the
                // surface restriction (other stmts surface a
                // clean diagnostic with the workaround). The
                // tail expression's value is the block's value.
                // Closure #129 extends the v1 Block MVP.
                let open_span = token.span;
                let mut stmts: Vec<Stmt> = Vec::new();
                loop {
                    if self.check(|k| matches!(k, TokenKind::Let)) {
                        stmts.push(self.parse_let_stmt()?);
                    } else if self.check(|k| matches!(k, TokenKind::Print)) {
                        stmts.push(self.parse_print_stmt()?);
                    } else {
                        break;
                    }
                }
                let tail = self.parse_expr()?;
                let close = self.expect_keyword(
                    "'}' (block-expression close)",
                    |k| matches!(k, TokenKind::RBrace),
                )?;
                Ok(Expr {
                    kind: ExprKind::Block {
                        stmts,
                        tail: Box::new(tail),
                    },
                    span: open_span.merge(close.span),
                })
            }
            TokenKind::Try => {
                // `try EXPR` — parse inner at call-expr
                // precedence so common forms like
                // `try maybe(5)` or `try Type.helper(args)`
                // bind correctly (without this, `try EXPR`
                // stopped at primary level and the outer
                // postfix `(...)` parser saw the try as the
                // callee, surfacing "only named functions
                // can be called"). Binary `+`/`*` etc. stay
                // outside the try by binding above
                // parse_call_expr's precedence.
                let inner = self.parse_call_expr()?;
                let inner_span = inner.span;
                Ok(Expr {
                    kind: ExprKind::Try { inner: Box::new(inner) },
                    span: token.span.merge(inner_span),
                })
            }
            TokenKind::If => {
                // If-expression: `if cond { expr } else { expr }`.
                // Both branches must be a single expression in
                // braces. Statement-bearing if blocks stay in
                // parse_stmt (which sees `if` at statement
                // position before parse_expr is invoked).
                let cond = self.parse_expr()?;
                self.expect_keyword("'{' (if-expression then-branch)", |k| {
                    matches!(k, TokenKind::LBrace)
                })?;
                let then_value = self.parse_expr()?;
                self.expect_keyword("'}' (if-expression then-branch)", |k| {
                    matches!(k, TokenKind::RBrace)
                })?;
                self.expect_keyword("'else' (if-expression)", |k| {
                    matches!(k, TokenKind::Else)
                })?;
                // `else if cond { … }` chains — the `else`
                // branch is itself an if-expression. Allow
                // `if cond { e1 } else if cond2 { e2 } else
                // { e3 }` as a single nested if-expression
                // tree.
                let (else_value, close_span) =
                    if self.check(|k| matches!(k, TokenKind::If)) {
                        let nested = self.parse_primary_expr()?;
                        let nested_span = nested.span;
                        (nested, nested_span)
                    } else {
                        self.expect_keyword(
                            "'{' (if-expression else-branch)",
                            |k| matches!(k, TokenKind::LBrace),
                        )?;
                        let v = self.parse_expr()?;
                        let close = self.expect_keyword(
                            "'}' (if-expression else-branch)",
                            |k| matches!(k, TokenKind::RBrace),
                        )?;
                        (v, close.span)
                    };
                Ok(Expr {
                    kind: ExprKind::IfExpr {
                        cond: Box::new(cond),
                        then_value: Box::new(then_value),
                        else_value: Box::new(else_value),
                    },
                    span: token.span.merge(close_span),
                })
            }
            TokenKind::Match => {
                let scrutinee = self.parse_expr()?;
                self.expect_keyword("'{'", |k| matches!(k, TokenKind::LBrace))?;
                let mut arms = Vec::new();
                while !self.check(|k| matches!(k, TokenKind::RBrace | TokenKind::Eof)) {
                    // Five pattern shapes in v1:
                    //   - `_` wildcard
                    //   - `42` / `-1` integer literal
                    //   - `true` / `false` bool literal
                    //   - `"foo"` string literal
                    //   - `EnumName.VariantName` variant
                    // Dispatch on the first token: Minus or
                    // Int → integer; True/False → bool; Str
                    // → string; identifier `_` → wildcard;
                    // any other identifier → variant.
                    // T1.3 wildcard + integer-literal pattern.
                    let (pattern, pat_span) = if self
                        .check(|k| matches!(k, TokenKind::True))
                    {
                        let tok = self.bump();
                        (Pattern::Bool(true), tok.span)
                    } else if self.check(|k| matches!(k, TokenKind::False)) {
                        let tok = self.bump();
                        (Pattern::Bool(false), tok.span)
                    } else if self.check(|k| matches!(k, TokenKind::Str(_))) {
                        let tok = self.bump();
                        let span = tok.span;
                        let text = match tok.kind {
                            TokenKind::Str(s) => s,
                            _ => unreachable!(),
                        };
                        (Pattern::Str(text), span)
                    } else if self
                        .check(|k| matches!(k, TokenKind::Minus | TokenKind::Int(_)))
                    {
                        let pat_start = self.current().span;
                        let mut negative = false;
                        if self
                            .match_token(|k| matches!(k, TokenKind::Minus))
                            .is_some()
                        {
                            negative = true;
                        }
                        let int_tok = self.bump();
                        let int_span = int_tok.span;
                        let value = match int_tok.kind {
                            TokenKind::Int(v) => {
                                if negative {
                                    match v.checked_neg() {
                                        Some(neg) => neg,
                                        None => {
                                            return Err(Diagnostic::new(
                                                pat_start.merge(int_span),
                                                "integer pattern overflow when negating",
                                            ));
                                        }
                                    }
                                } else {
                                    v
                                }
                            }
                            _ => {
                                return Err(Diagnostic::new(
                                    int_span,
                                    "expected integer literal in match pattern",
                                ));
                            }
                        };
                        (Pattern::Int(value), pat_start.merge(int_span))
                    } else {
                        let first_tok = self.expect_ident()?;
                        let pat_start = first_tok.span;
                        let first_text = ident_text(first_tok);
                        if first_text == "_" {
                            (Pattern::Wildcard, pat_start)
                        } else {
                            self.expect_keyword(
                                "'.' (variant access in match pattern)",
                                |k| matches!(k, TokenKind::Dot),
                            )?;
                            let variant_tok = self.expect_ident()?;
                            let mut pat_span = pat_start.merge(variant_tok.span);
                            let variant = ident_text(variant_tok);
                            // Optional `(binding)` after the variant
                            // name — payloaded destructure. T1.3
                            // phase 2b. v1 accepts the single-binding
                            // form (`Some(x)`) only; multi-binding
                            // tuple-style destructure is deferred.
                            if self.check(|k| matches!(k, TokenKind::LParen)) {
                                self.bump();
                                let binding_tok = self.expect_ident()?;
                                let binding = ident_text(binding_tok);
                                let close = self.expect_keyword(
                                    "')' (variant payload binding close)",
                                    |k| matches!(k, TokenKind::RParen),
                                )?;
                                pat_span = pat_start.merge(close.span);
                                (
                                    Pattern::VariantWithBinding {
                                        enum_name: first_text,
                                        variant,
                                        binding,
                                    },
                                    pat_span,
                                )
                            } else {
                                (
                                    Pattern::Variant {
                                        enum_name: first_text,
                                        variant,
                                    },
                                    pat_span,
                                )
                            }
                        }
                    };
                    self.expect_keyword("'then'", |k| matches!(k, TokenKind::Then))?;
                    let body = self.parse_expr()?;
                    arms.push(MatchArm {
                        pattern,
                        pattern_span: pat_span,
                        body,
                    });
                    // Comma between arms required; trailing
                    // comma before `}` allowed.
                    if self.match_token(|k| matches!(k, TokenKind::Comma)).is_none() {
                        break;
                    }
                }
                let close = self.expect_keyword(
                    "'}' (match expression)",
                    |k| matches!(k, TokenKind::RBrace),
                )?;
                Ok(Expr {
                    kind: ExprKind::Match {
                        scrutinee: Box::new(scrutinee),
                        arms,
                    },
                    span: token.span.merge(close.span),
                })
            }
            TokenKind::Ident(first_name) => {
                // Closure #242: path expression
                // `module::item`. v1 supports a single `::`
                // (no nested modules). The resulting `name`
                // is the joined path string; later parser
                // logic (struct literal / call / var) uses
                // it unchanged. The checker recognizes
                // `::` in identifier names and routes
                // through module resolution.
                let mut name = first_name.clone();
                let mut name_span = token.span;
                if self.check(|k| matches!(k, TokenKind::ColonColon)) {
                    self.bump(); // consume ::
                    let next_tok = self.expect_ident()?;
                    let next_span = next_tok.span;
                    let next_name = ident_text(next_tok);
                    // Internal name uses `__` (backend-safe
                    // identifier) instead of `::`. The lexer's
                    // ColonColon token marks the module
                    // boundary at parse time; downstream
                    // (checker + backends) sees the
                    // sanitized form. Diagnostics would
                    // ideally preserve the source spelling
                    // via spans, but for v1 the `__` form
                    // appears in error messages.
                    name = format!("{}__{}", name, next_name);
                    name_span = name_span.merge(next_span);
                }
                // Struct literal `Name { field: val, … }` —
                // we recognize the shape by looking past
                // `{` for `ident :`. Anything else means
                // we leave the identifier alone (block,
                // var). The capitalization convention
                // (struct names start uppercase) gates the
                // attempt so plain variables never trip the
                // lookahead. T1.2.
                // For module-qualified names like `foo__Point`
                // the LAST segment's capitalization is what
                // counts.
                let last_segment: &str = name.rsplit("__").next().unwrap_or(&name);
                let starts_uppercase = last_segment
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_uppercase())
                    .unwrap_or(false);
                let starts_with_lbrace = matches!(self.current().kind, TokenKind::LBrace);
                let inner_is_field = matches!(
                    self.tokens.get(self.pos + 1).map(|t| &t.kind),
                    Some(TokenKind::Ident(_))
                ) && matches!(
                    self.tokens.get(self.pos + 2).map(|t| &t.kind),
                    Some(TokenKind::Colon)
                );
                let inner_is_empty = matches!(
                    self.tokens.get(self.pos + 1).map(|t| &t.kind),
                    Some(TokenKind::RBrace)
                );
                let looks_like_struct = starts_uppercase
                    && starts_with_lbrace
                    && (inner_is_field || inner_is_empty);
                if looks_like_struct {
                    self.bump(); // {
                    let mut fields = Vec::new();
                    while !self.check(|k| matches!(k, TokenKind::RBrace | TokenKind::Eof)) {
                        let fname_tok = self.expect_ident()?;
                        let fname = ident_text(fname_tok);
                        self.expect_keyword("':'", |k| matches!(k, TokenKind::Colon))?;
                        let value = self.parse_expr()?;
                        fields.push((fname, value));
                        if self.match_token(|k| matches!(k, TokenKind::Comma)).is_none() {
                            break;
                        }
                    }
                    let close = self.expect_keyword(
                        "'}' (struct literal)",
                        |k| matches!(k, TokenKind::RBrace),
                    )?;
                    return Ok(Expr {
                        kind: ExprKind::StructLit {
                            type_name: name,
                            type_name_span: name_span,
                            fields,
                        },
                        span: name_span.merge(close.span),
                    });
                }
                Ok(Expr {
                    kind: ExprKind::Var(name),
                    span: name_span,
                })
            }
            TokenKind::LParen => {
                // Parenthesized form: either grouped expression
                // `(e)` or tuple `(e1, e2, …)`. Disambiguate
                // on the comma after the first sub-expression.
                let first = self.parse_expr()?;
                if self
                    .match_token(|k| matches!(k, TokenKind::Comma))
                    .is_some()
                {
                    let mut elements = vec![first];
                    loop {
                        // Trailing comma allowed: stop if we
                        // see `)` right after a comma.
                        if self.check(|k| matches!(k, TokenKind::RParen)) {
                            break;
                        }
                        elements.push(self.parse_expr()?);
                        if self
                            .match_token(|k| matches!(k, TokenKind::Comma))
                            .is_none()
                        {
                            break;
                        }
                    }
                    let close = self.expect_keyword(
                        "')'",
                        |k| matches!(k, TokenKind::RParen),
                    )?;
                    return Ok(Expr {
                        kind: ExprKind::Tuple(elements),
                        span: token.span.merge(close.span),
                    });
                }
                self.expect_keyword("')'", |kind| matches!(kind, TokenKind::RParen))?;
                Ok(first)
            }
            TokenKind::LBracket => {
                let mut elements = Vec::new();
                if !self.check(|kind| matches!(kind, TokenKind::RBracket)) {
                    loop {
                        elements.push(self.parse_expr()?);
                        if self
                            .match_token(|kind| matches!(kind, TokenKind::Comma))
                            .is_none()
                        {
                            break;
                        }
                        // Allow a trailing comma before `]`
                        // so multi-line array literals can use
                        // the same comma-on-every-line style
                        // as struct/enum/methods blocks.
                        if self.check(|k| matches!(k, TokenKind::RBracket)) {
                            break;
                        }
                    }
                }
                let close =
                    self.expect_keyword("']'", |kind| matches!(kind, TokenKind::RBracket))?;
                Ok(Expr {
                    kind: ExprKind::ArrayLit { elements },
                    span: token.span.merge(close.span),
                })
            }
            TokenKind::Len => {
                self.expect_keyword("'('", |kind| matches!(kind, TokenKind::LParen))?;
                let array = self.parse_expr()?;
                let close =
                    self.expect_keyword("')'", |kind| matches!(kind, TokenKind::RParen))?;
                Ok(Expr {
                    kind: ExprKind::Len {
                        array: Box::new(array),
                    },
                    span: token.span.merge(close.span),
                })
            }
            // `min(a, b)` / `max(a, b)` no longer get a
            // dedicated parse arm — they're regular
            // identifier calls that the checker dispatches
            // to the intrinsic helper based on the name.
            // This frees `min` / `max` as legal field /
            // local names outside the reduction-op context.
            _ => Err(Diagnostic::new(token.span, "expected expression")),
        }
    }

    fn current_binary_op(&self) -> Option<(BinaryOp, u8)> {
        // Precedence follows Rust: `|` < `^` < `&` < shifts < `+/-`
        // < `*//%`. Comparisons sit above `&&`/`||` and below the
        // bitwise ops, so `a == b | c` parses as `a == (b | c)`.
        // `&` doubles as the prefix reference operator; the unary
        // path is handled separately in `parse_unary_expr`, so
        // listing `Amp` here only affects infix positions.
        match self.current().kind {
            TokenKind::OrOr => Some((BinaryOp::Or, 1)),
            TokenKind::AndAnd => Some((BinaryOp::And, 2)),
            TokenKind::EqEq => Some((BinaryOp::Eq, 3)),
            TokenKind::BangEq => Some((BinaryOp::Ne, 3)),
            TokenKind::Less => Some((BinaryOp::Lt, 4)),
            TokenKind::LessEq => Some((BinaryOp::Le, 4)),
            TokenKind::Greater => Some((BinaryOp::Gt, 4)),
            TokenKind::GreaterEq => Some((BinaryOp::Ge, 4)),
            TokenKind::Pipe => Some((BinaryOp::BitOr, 5)),
            TokenKind::Caret => Some((BinaryOp::BitXor, 6)),
            TokenKind::Amp => Some((BinaryOp::BitAnd, 7)),
            TokenKind::LessLess => Some((BinaryOp::Shl, 8)),
            TokenKind::GreaterGreater => Some((BinaryOp::Shr, 8)),
            TokenKind::Plus => Some((BinaryOp::Add, 9)),
            TokenKind::Minus => Some((BinaryOp::Sub, 9)),
            TokenKind::Star => Some((BinaryOp::Mul, 10)),
            TokenKind::Slash => Some((BinaryOp::Div, 10)),
            TokenKind::Percent => Some((BinaryOp::Rem, 10)),
            _ => None,
        }
    }

    fn expect_ident(&mut self) -> Result<Token, Diagnostic> {
        if self.check(|kind| matches!(kind, TokenKind::Ident(_))) {
            Ok(self.bump())
        } else {
            Err(self.error_here("expected identifier"))
        }
    }

    fn expect_string(&mut self) -> Result<Token, Diagnostic> {
        if self.check(|kind| matches!(kind, TokenKind::Str(_))) {
            Ok(self.bump())
        } else {
            Err(self.error_here("expected string literal"))
        }
    }

    fn expect_close_angle(&mut self) -> Result<(), Diagnostic> {
        let current_kind = self.current().kind.clone();
        match current_kind {
            TokenKind::Greater => {
                self.bump();
                Ok(())
            }
            TokenKind::GreaterGreater => {
                // Split `>>` into `>` + `>` so nested `Vec<Vec<T>>` parses.
                let span = self.current().span;
                let split_start = span.start + 1;
                self.tokens[self.pos] = Token {
                    kind: TokenKind::Greater,
                    span: crate::span::Span::new(split_start, span.end),
                };
                Ok(())
            }
            _ => Err(self.error_here("expected '>'")),
        }
    }

    fn expect_keyword(
        &mut self,
        expected: &'static str,
        predicate: impl FnOnce(&TokenKind) -> bool,
    ) -> Result<Token, Diagnostic> {
        if predicate(&self.current().kind) {
            Ok(self.bump())
        } else {
            Err(self.error_here(format!("expected {}", expected)))
        }
    }

    fn match_token(&mut self, predicate: impl FnOnce(&TokenKind) -> bool) -> Option<Token> {
        if predicate(&self.current().kind) {
            Some(self.bump())
        } else {
            None
        }
    }

    fn check(&self, predicate: impl FnOnce(&TokenKind) -> bool) -> bool {
        predicate(&self.current().kind)
    }

    fn current(&self) -> &Token {
        &self.tokens[self.pos]
    }

    fn bump(&mut self) -> Token {
        let token = self.current().clone();
        if !matches!(token.kind, TokenKind::Eof) {
            self.pos += 1;
        }
        token
    }

    fn error_here(&self, message: impl Into<String>) -> Diagnostic {
        Diagnostic::new(self.current().span, message)
    }
}

fn ident_text(token: Token) -> String {
    match token.kind {
        TokenKind::Ident(name) => name,
        _ => unreachable!("expected identifier"),
    }
}

/// Recognize integer-literal initializers, including
/// arithmetic over previously-declared consts. Used by
/// `parse_const_decl` to stash literal int values for the
/// `[T; SIZE]` array-length resolver. Mirrors the checker's
/// `literal_const_value` const-fold. T0.0 follow-up.
fn expr_as_int_literal(
    expr: &Expr,
    prior_consts: &std::collections::HashMap<String, i128>,
) -> Option<i128> {
    match &expr.kind {
        ExprKind::Int(v) => Some(*v),
        ExprKind::Var(name) => prior_consts.get(name).copied(),
        ExprKind::Unary { op: UnaryOp::Neg, expr: inner } => {
            expr_as_int_literal(inner, prior_consts)?.checked_neg()
        }
        ExprKind::Binary { op, left, right } => {
            let l = expr_as_int_literal(left, prior_consts)?;
            let r = expr_as_int_literal(right, prior_consts)?;
            match op {
                BinaryOp::Add => l.checked_add(r),
                BinaryOp::Sub => l.checked_sub(r),
                BinaryOp::Mul => l.checked_mul(r),
                BinaryOp::Div if r != 0 => l.checked_div(r),
                BinaryOp::Rem if r != 0 => l.checked_rem(r),
                _ => None,
            }
        }
        _ => None,
    }
}
