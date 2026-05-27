use crate::diagnostic::Diagnostic;
use crate::span::Span;

#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

#[derive(Clone, Debug, PartialEq)]
pub enum TokenKind {
    Ident(String),
    Int(i128),
    Float(f64),
    Str(String),
    Fn,
    /// `pure` function modifier: keyword that precedes `fn`.
    /// Marks the function as side-effect-free.
    Pure,
    /// `extern "C" fn name(params) -> R;` — FFI declaration
    /// (closure #269). The body is supplied by an externally-
    /// linked object file; the checker registers the signature
    /// only and never validates a body. Calls to the fn emit
    /// the bare C-ABI symbol (no `fn_` prefix) so external
    /// linkers find it.
    Extern,
    /// `parallel` loop modifier: keyword that precedes `for`.
    /// Marks the iteration as independently parallelizable
    /// (verified by the effects checker).
    Parallel,
    /// `reduce <var> with <op>;` clause on a `parallel for`. The
    /// body must update `<var>` only via the named op; each thread
    /// accumulates a partial value and the runtime combines them.
    Reduce,
    /// Part of the `reduce <var> with <op>;` clause syntax.
    With,
    /// `min` reduction op + builtin function `min(a, b)`.
    Min,
    /// `max` reduction op + builtin function `max(a, b)`.
    Max,
    /// `task <name> { ... }` — declares an affine handle of type
    /// `Task` and a side-effect-free body that runs once. v1
    /// lowers sequentially; the verifier is the value-add.
    Task,
    /// `join <name>;` — consumes a `Task` handle. v1 lowers to a
    /// no-op once the spawn's body has executed.
    Join,
    Let,
    Return,
    If,
    Else,
    While,
    Break,
    Continue,
    Mut,
    For,
    In,
    /// `ref x` — prefix borrow operator. Replaces the older
    /// `&x` shape; the same keyword is used in type position
    /// (`ref T`) and at call-site / for-iter borrows. Refines
    /// T0.0 of the consolidated TODO.
    Ref,
    /// `struct Name { f1: T1, … }` — top-level record-type
    /// declaration. T1.2.
    Struct,
    /// `enum Name { Variant1, Variant2, … }` — top-level
    /// tagged-union declaration. T1.3.
    Enum,
    /// `match expr { Pat then expr, … }` — pattern-match
    /// expression. T1.3.
    Match,
    /// `Pattern then body` — match-arm separator. T1.3.
    Then,
    /// `interface Name { fn …; }` — abstract behavior
    /// declaration. T1.5.
    Interface,
    /// `implement Iface for Type { … }` — bind interface
    /// methods to a concrete type. T1.5.
    Implement,
    /// `where T is Iface` — generic bound clause. T1.5.
    Where,
    /// `T is Iface` — bound predicate keyword. T1.5.
    Is,
    /// `const NAME: T = expr;` — top-level compile-time
    /// constant. v1 restricts the initializer to a literal
    /// expression and the type to Copy. T4.15.
    Const,
    /// `type Name = Type;` — top-level type alias. v1
    /// rejects recursive aliases. T4.15 (type-alias half).
    Type,
    /// `methods on TypeName { fn foo(self: …) -> … { … } }`
    /// — group of methods attached to a concrete type.
    /// Method bodies lower to free functions with names
    /// mangled as `<TypeName>_<methodName>`, so callers can
    /// write `p.foo(args)` and have the checker rewrite the
    /// MethodCall into the mangled call. T1.2 phase 2a.
    Methods,
    /// `from EXPR` — opening of the range form
    /// `from <start> to <end>` used by `for` / `parallel for`.
    /// Replaces `<start>..<end>`. T0.0.
    From,
    /// `to EXPR` — closing of the range form (and future slice
    /// shape `xs[lo to hi]`). T0.0.
    To,
    DotDot,
    /// `.` — field access (`p.x`) and tuple-index (`t.0`)
    /// postfix operator. Distinct from `DotDot`. T1.1 / T1.2.
    Dot,
    Intent,
    Use,
    Requires,
    Ensures,
    Invariant,
    Assert,
    Prove,
    Print,
    /// `try EXPR` — error-propagation sugar over payloaded
    /// enums. If `EXPR` evaluates to the enum's payload-less
    /// "early-return" variant (e.g. `Opt.None`), the enclosing
    /// function returns that value immediately. Otherwise the
    /// payload is extracted and becomes the value of the `try`
    /// expression. Requires the enclosing function's return
    /// type to match the enum type. T2.6.
    Try,
    Len,
    As,
    True,
    False,
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
    Vec,
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Colon,
    ColonColon,
    Semicolon,
    Comma,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Bang,
    Equal,
    EqEq,
    BangEq,
    Less,
    LessEq,
    LessLess,
    Greater,
    GreaterEq,
    GreaterGreater,
    Amp,
    AndAnd,
    Pipe,
    OrOr,
    Caret,
    Arrow,
    /// `module name { ... }` — namespace declaration (closure
    /// #242). vāṇī uses Rust-style modules: explicit paths
    /// with `::` separator, `pub` for export, private-by-default
    /// inside the module. Top-level items stay globally visible
    /// for back-compat.
    Module,
    /// `pub` modifier: makes an item visible from outside its
    /// module. Default visibility for module-scoped items is
    /// private. Top-level items (not inside any `module`) stay
    /// globally visible.
    Pub,
    Eof,
}

/// Resolve a Devanagari keyword alias to its English-equivalent
/// `TokenKind`. Returns `None` for any non-alias string, which the
/// caller treats as a regular Unicode identifier name.
///
/// V1 ships a small first cut covering the most common control-flow
/// and verification keywords across Sanskrit / Hindi / Marathi.
/// Conflicts where the same Devanagari word would map to two
/// different English keywords are resolved in favor of the most
/// idiomatic single-word form; multi-word aliases (e.g. `के लिए`
/// for `for`, `नहीं तो` for `else`) are deferred until the lexer
/// gains lookahead over whitespace.
///
/// The table is intentionally conservative — finalized aliases per
/// language will land with grammar consultant review per Roadmap
/// item #9.
fn devanagari_keyword(text: &str) -> Option<TokenKind> {
    let kind = match text {
        // fn
        "फलन" => TokenKind::Fn,           // phalan (Hindi/Marathi: "function")
        "कार्य" => TokenKind::Fn,         // kārya (Sanskrit/Marathi: "function/work")
        // let
        "मान" => TokenKind::Let,          // māna (Marathi: "assume/let")
        "माना" => TokenKind::Let,         // mānā (Sanskrit/Hindi)
        // return
        "परत" => TokenKind::Return,       // parat (Marathi: "back")
        "लौटाओ" => TokenKind::Return,     // lauṭāo (Hindi: "return!")
        "पुनरागम" => TokenKind::Return,   // punarāgama (Sanskrit)
        // if / else
        "यदि" => TokenKind::If,           // yadi (Sanskrit/Hindi: "if")
        "अगर" => TokenKind::If,           // agar (Hindi: "if")
        "जर" => TokenKind::If,            // jar (Marathi: "if")
        "अन्यथा" => TokenKind::Else,      // anyathā (Sanskrit: "else")
        "वरना" => TokenKind::Else,         // varnā (Hindi: "otherwise") — closure #267
        "नाहीतर" => TokenKind::Else,      // nāhītar (Marathi: "else")
        // while
        "यावत्" => TokenKind::While,      // yāvat (Sanskrit: "while/until")
        "जबतक" => TokenKind::While,       // jab tak (Hindi: "until")
        "जोपर्यंत" => TokenKind::While,   // jopa­ryanta (Marathi: "until")
        // for
        "प्रति" => TokenKind::For,        // prati (Sanskrit: "for each")
        "साठी" => TokenKind::For,         // sāṭhī (Marathi: "for")
        // match arm "then"
        "तदा" => TokenKind::Then,         // tadā (Sanskrit: "then")
        "तो" => TokenKind::Then,          // to (Hindi: "then")
        "तर" => TokenKind::Then,          // tar (Marathi: "then")
        // ref
        "पहा" => TokenKind::Ref,          // pahā (Marathi: "see/look")
        "देखो" => TokenKind::Ref,         // dekho (Hindi: "see!")
        // mut — closure #267 fills Sanskrit + Hindi gaps
        "बदल" => TokenKind::Mut,          // badla (Marathi root: "change")
        "परिवर्तनीय" => TokenKind::Mut,   // parivartanīya (Sanskrit/Hindi: "mutable")
        // match
        "जुळवा" => TokenKind::Match,      // juḷvā (Marathi: "match")
        "मिलान" => TokenKind::Match,      // milān (Hindi: "match")
        "मेल" => TokenKind::Match,        // mela (Sanskrit: "join/match")
        // assert
        "खात्री" => TokenKind::Assert,    // khātrī (Marathi: "certainty")
        "सुनिश्चित" => TokenKind::Assert, // sunishchit (Hindi: "ensured")
        "सिद्धम्" => TokenKind::Assert,   // siddham (Sanskrit)
        // prove — closure #267 fills Hindi + Marathi single-word
        "सिद्ध" => TokenKind::Prove,      // siddha (Sanskrit root)
        "प्रमाण" => TokenKind::Prove,     // pramāṇa (Sanskrit: "proof")
        "प्रमाणित" => TokenKind::Prove,   // pramāṇita (Hindi/Marathi: "proven")
        "दर्शाओ" => TokenKind::Prove,     // darśāo (Hindi imperative: "show!")
        "दाखवा" => TokenKind::Prove,      // dākhvā (Marathi imperative: "show!")
        // requires / ensures
        "अपेक्षित" => TokenKind::Requires, // apekṣita (Sanskrit: "required")
        "चाहिए" => TokenKind::Requires,    // cāhiye (Hindi: "needs")
        "पाहिजे" => TokenKind::Requires,   // pāhije (Marathi: "needs")
        // ensures — `निश्चित` shared Hindi/Marathi; add a Sanskrit
        // alternate. Closure #267.
        "निश्चित" => TokenKind::Ensures,   // nishchit (Hindi/Marathi: "definite")
        "सुनिश्चयित" => TokenKind::Ensures, // sunischayita (Sanskrit: "ensured")
        // bool literals — `सत्य/असत्य` are tatsama (Sanskrit
        // loanwords) widely used in all three languages. Add
        // colloquial Hindi/Marathi alternates. Closure #267.
        "सत्य" => TokenKind::True,         // satya (Sanskrit, shared)
        "सही" => TokenKind::True,          // sahī (Hindi/Marathi colloquial: "correct")
        "असत्य" => TokenKind::False,       // asatya (Sanskrit, shared)
        "अशुद्ध" => TokenKind::False,      // aśuddha (Hindi/Marathi: "incorrect")
        // print / write — `लिख` (likh, root for "write") +
        // imperative `लिखो` (likho, "write!"). `छाप` (chāp,
        // "imprint/stamp") was the previous spelling but
        // feels off for screen output; removed in favor of
        // the natural "write" verb across all three
        // Devanagari-script languages.
        "लिख" => TokenKind::Print,         // likh (Sanskrit root: "write")
        "लिखो" => TokenKind::Print,        // likho (Hindi/Marathi imperative: "write!")
        // pure — `शुद्ध` is tatsama, shared across all three.
        "शुद्ध" => TokenKind::Pure,        // śuddha (Sanskrit/Hindi/Marathi: "pure")
        // struct / enum — closure #267 fills gaps. `संरचना`
        // is tatsama and works in Marathi too.
        "संरचना" => TokenKind::Struct,     // saṁracanā (Sanskrit/Hindi/Marathi: "structure")
        "विकल्प" => TokenKind::Enum,       // vikalpa (Sanskrit: "option/alternative")
        "गणन" => TokenKind::Enum,          // gaṇan (Hindi/Marathi: "enumeration")
        // const
        "स्थिर" => TokenKind::Const,       // sthira (Sanskrit/Hindi/Marathi: "fixed/constant")
        "नियत" => TokenKind::Const,        // niyat (Hindi/Marathi: "fixed/determined")
        // break / continue
        "विराम" => TokenKind::Break,       // virāma (Sanskrit: "pause/stop")
        "रुको" => TokenKind::Break,        // ruko (Hindi: "stop")
        "थांब" => TokenKind::Break,        // thāmba (Marathi: "stop")
        "अग्रे" => TokenKind::Continue,    // agre (Sanskrit: "forward") — closure #267
        "पुढे" => TokenKind::Continue,     // puḍhe (Marathi: "ahead/onward")
        "आगे" => TokenKind::Continue,      // āge (Hindi: "ahead")
        // for-loop range words
        "में" => TokenKind::In,             // meṁ (Hindi: "in")
        "से" => TokenKind::From,           // se (Hindi: "from")
        "तक" => TokenKind::To,             // tak (Hindi: "to/until")
        // reduce / with for `parallel for X reduce Y with op` —
        // `संक्षेप` / `सह` are tatsama and work in all three.
        "संक्षेप" => TokenKind::Reduce,    // saṁkṣepa (Sanskrit/Hindi/Marathi: "reduction")
        "सह" => TokenKind::With,           // saha (Sanskrit/Hindi/Marathi: "with")
        // parallel — closure #267 adds a single-word alias
        // (the existing multi-word `समान्तर प्रति` stays for
        // back-compat with Sanskrit-style writing).
        "समानांतर" => TokenKind::Parallel, // samānāntara (Sanskrit/Hindi/Marathi: "parallel")
        // Closure #267: namespace + concurrency keywords now
        // have Devanagari aliases. These are technical terms
        // with no single natural translation in any of the
        // three languages; we pick a Sanskrit-root form that
        // works as tatsama (loanword) in Hindi and Marathi too.
        // `kosh` (कोश, "treasure/repository") is already vāṇी's
        // name for the crate concept — aliased at the parser
        // level via `pub(kosh)` syntax, not at the lexer.
        //
        // use / module / pub / as — namespace imports
        "उपयोग" => TokenKind::Use,         // upayog (Sanskrit/Hindi/Marathi: "use")
        "खण्ड" => TokenKind::Module,       // khaṇḍa (Sanskrit/Hindi/Marathi: "section/module")
        "मॉड्यूल" => TokenKind::Module,    // mōḍyūla (Hindi/Marathi loanword: "module")
        "सार्वजनिक" => TokenKind::Pub,     // sārvajanik (Sanskrit/Hindi/Marathi: "public")
        "यथा" => TokenKind::As,            // yathā (Sanskrit/Hindi/Marathi: "as/like")
        // interface / implement / methods
        "संकेत" => TokenKind::Interface,   // saṅket (Sanskrit/Hindi/Marathi: "protocol/sign")
        "अंतरापृष्ठ" => TokenKind::Interface, // antarāpṛṣṭha (Sanskrit literal: "inter-face")
        "कार्यान्वित" => TokenKind::Implement, // kāryānvit (Sanskrit/Hindi/Marathi: "to put into effect")
        "विधि" => TokenKind::Methods,       // vidhi (Sanskrit/Hindi/Marathi: "method/procedure")
        // where / is — for generic bounds (`where T is Trait`)
        "जहाँ" => TokenKind::Where,        // jahām̐ (Hindi: "where")
        "यत्र" => TokenKind::Where,        // yatra (Sanskrit: "where")
        "जिथे" => TokenKind::Where,        // jithe (Marathi: "where")
        "है" => TokenKind::Is,             // hai (Hindi: "is")
        "अस्ति" => TokenKind::Is,          // asti (Sanskrit: "is")
        "आहे" => TokenKind::Is,            // āhe (Marathi: "is")
        // try (Rust `?` operator analog) / task / join
        "प्रयास" => TokenKind::Try,        // prayās (Sanskrit/Hindi/Marathi: "attempt")
        "नियोग" => TokenKind::Task,        // niyog (Sanskrit/Hindi/Marathi: "assignment/task")
        "संयोजन" => TokenKind::Join,       // saṁyojan (Sanskrit/Hindi/Marathi: "joining")
        _ => return None,
    };
    Some(kind)
}

pub fn lex(source: &str) -> Result<Vec<Token>, Diagnostic> {
    let mut tokens = Lexer::new(source).lex()?;
    merge_multi_word_devanagari_aliases(&mut tokens, source);
    merge_give_back_ascii_alias(&mut tokens, source);
    enforce_language_purity(&tokens, source)?;
    Ok(tokens)
}

/// Per-file language purity gate (closure #236). vāṇī supports
/// English structure keywords (`fn`, `let`, `return`, …) and a
/// Devanagari alias table covering Sanskrit / Hindi / Marathi.
/// A file should commit to ONE script: mixing the English form
/// with Devanagari forms in the same file surfaces as a clear
/// "language mismatch" diagnostic so the reader doesn't have to
/// mentally parse two structure-keyword systems at once.
///
/// V1 enforces script-level purity (English vs Devanagari).
/// Finer-grained Sanskrit / Hindi / Marathi distinction within
/// Devanagari is deferred — the existing alias table maps some
/// words ambiguously (e.g. `यदि` is both Sanskrit and Hindi).
/// Grammar-consultant review is the gate for that next step.
///
/// Type names (`i64`, `bool`, `Vec`, …) and the boolean literals
/// (`true`/`false`) stay neutral so a Hindi file can still write
/// `फलन add(a: i64, b: i64) -> i64`. The gate looks only at
/// structure keywords.
fn enforce_language_purity(tokens: &[Token], source: &str) -> Result<(), Diagnostic> {
    let mut english_keyword: Option<Span> = None;
    let mut devanagari_keyword: Option<Span> = None;
    for tok in tokens {
        if !is_structure_keyword_kind(&tok.kind) {
            continue;
        }
        let text = &source[tok.span.start..tok.span.end];
        let is_devanagari = text.chars().any(|c| {
            // Devanagari Unicode range plus its extension.
            ('\u{0900}'..='\u{097F}').contains(&c)
                || ('\u{A8E0}'..='\u{A8FF}').contains(&c)
        });
        if is_devanagari {
            if devanagari_keyword.is_none() {
                devanagari_keyword = Some(tok.span);
            }
            if let Some(eng_span) = english_keyword {
                return Err(Diagnostic::new(
                    tok.span,
                    format!(
                        "language mismatch: file already used an English \
                         structure keyword (see span {}..{}), can't switch \
                         to a Devanagari alias mid-file. Pick one language \
                         per file.",
                        eng_span.start, eng_span.end
                    ),
                ));
            }
        } else {
            if english_keyword.is_none() {
                english_keyword = Some(tok.span);
            }
            if let Some(dev_span) = devanagari_keyword {
                return Err(Diagnostic::new(
                    tok.span,
                    format!(
                        "language mismatch: file already used a Devanagari \
                         structure keyword (see span {}..{}), can't switch \
                         to an English keyword mid-file. Pick one language \
                         per file.",
                        dev_span.start, dev_span.end
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Returns true when the token is a *structure* keyword — the
/// kind that should be subject to the language-purity gate.
/// Type names, literals, identifiers, operators, and the
/// boolean literals stay neutral so they can appear in any
/// language file. Add new structure keywords here when extending
/// the lexer.
fn is_structure_keyword_kind(kind: &TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Fn
            | TokenKind::Pure
            | TokenKind::Extern
            | TokenKind::Parallel
            | TokenKind::Reduce
            | TokenKind::With
            | TokenKind::Task
            | TokenKind::Join
            | TokenKind::Let
            | TokenKind::Return
            | TokenKind::If
            | TokenKind::Else
            | TokenKind::While
            | TokenKind::Break
            | TokenKind::Continue
            | TokenKind::Mut
            | TokenKind::For
            | TokenKind::In
            | TokenKind::Ref
            | TokenKind::From
            | TokenKind::To
            | TokenKind::Struct
            | TokenKind::Enum
            | TokenKind::Match
            | TokenKind::Then
            | TokenKind::Interface
            | TokenKind::Implement
            | TokenKind::Where
            | TokenKind::Is
            | TokenKind::Const
            | TokenKind::Type
            | TokenKind::Methods
            | TokenKind::Intent
            | TokenKind::Use
            | TokenKind::Requires
            | TokenKind::Ensures
            | TokenKind::Invariant
            | TokenKind::Assert
            | TokenKind::Prove
            | TokenKind::Print
            | TokenKind::Try
            | TokenKind::Module
            | TokenKind::Pub
    )
}

/// Post-lex pass that merges adjacent token pairs whose combined
/// text matches a multi-word Devanagari keyword alias. Examples:
/// Hindi `नहीं तो` (`nahīṁ to`, "else"), `के लिए` (`ke liye`,
/// "for"), `सिद्ध करो` (`siddha karo`, "prove"). The lexer's main
/// pass only sees whitespace-separated words, so multi-word
/// aliases need this stitching after the fact.
///
/// Reads the original source text via each token's span so it can
/// inspect words that were already resolved to single-word aliases
/// (e.g. `तो` lexed as `Then`). The multi-word form takes
/// precedence when both words are present and the combined string
/// matches a multi-word alias.
fn merge_multi_word_devanagari_aliases(tokens: &mut Vec<Token>, source: &str) {
    let mut i = 0;
    while i + 1 < tokens.len() {
        let a_span = tokens[i].span;
        let b_span = tokens[i + 1].span;
        // Skip merging across token gaps that contain more than
        // whitespace (the merger pattern is `WORD WORD` with only
        // ASCII spaces / tabs in between).
        if !whitespace_only(source, a_span.end, b_span.start) {
            i += 1;
            continue;
        }
        let a_text = source.get(a_span.start..a_span.end);
        let b_text = source.get(b_span.start..b_span.end);
        if let (Some(a), Some(b)) = (a_text, b_text) {
            // Both word slices must contain non-ASCII bytes (i.e.
            // they're Devanagari, not English keywords). Avoids
            // accidentally merging `let x` or similar.
            if a.bytes().any(|byte| byte >= 0x80)
                && b.bytes().any(|byte| byte >= 0x80)
            {
                let combined = format!("{} {}", a, b);
                if let Some(kind) = multi_word_devanagari_keyword(&combined) {
                    let merged_span = a_span.merge(b_span);
                    tokens[i] = Token { kind, span: merged_span };
                    tokens.remove(i + 1);
                    continue;
                }
            }
        }
        i += 1;
    }
}

/// Closure #255: fold the two-word ASCII phrase `give back`
/// into a single `Return` token. The lexer's main pass already
/// maps the standalone words `give` and `give_back` to
/// `TokenKind::Return`; this pass picks up the writer who
/// preferred two whitespace-separated words. We don't reuse
/// the Devanagari merger because it intentionally rejects
/// ASCII pairs to avoid accidentally merging unrelated
/// identifiers (e.g. `let x` would have collided with a
/// hypothetical `let x` alias). The pattern here is
/// specific: a `Return` token whose source text is exactly
/// `give`, followed by an `Ident` whose source text is
/// exactly `back`, with only whitespace between them. Real
/// `return back;` style code is unaffected because `return`
/// (the canonical form) doesn't trigger.
fn merge_give_back_ascii_alias(tokens: &mut Vec<Token>, source: &str) {
    let mut i = 0;
    while i + 1 < tokens.len() {
        if !matches!(tokens[i].kind, TokenKind::Return) {
            i += 1;
            continue;
        }
        if !matches!(tokens[i + 1].kind, TokenKind::Ident(_)) {
            i += 1;
            continue;
        }
        let a_span = tokens[i].span;
        let b_span = tokens[i + 1].span;
        if !whitespace_only(source, a_span.end, b_span.start) {
            i += 1;
            continue;
        }
        let a_text = source.get(a_span.start..a_span.end);
        let b_text = source.get(b_span.start..b_span.end);
        if matches!(a_text, Some("give")) && matches!(b_text, Some("back")) {
            // Extend the Return token's span to cover both
            // words so diagnostics underline the full phrase,
            // then drop the trailing `back`.
            let merged_span = a_span.merge(b_span);
            tokens[i] = Token {
                kind: TokenKind::Return,
                span: merged_span,
            };
            tokens.remove(i + 1);
            // Don't advance: the new token at `i` might be
            // followed by another mergeable pair (unlikely
            // but cheap to allow).
            continue;
        }
        i += 1;
    }
}

/// True iff `source[start..end]` contains only ASCII whitespace.
fn whitespace_only(source: &str, start: usize, end: usize) -> bool {
    source.get(start..end)
        .map(|s| s.bytes().all(|b| b == b' ' || b == b'\t'))
        .unwrap_or(false)
}

/// Resolve a multi-word Devanagari phrase to its English-equivalent
/// `TokenKind`. The merger only consults this when both words were
/// lexed as Devanagari Idents (i.e., neither was a single-word
/// alias on its own). For v1, this is the safe overlap because
/// none of these phrases share their first word with a single-word
/// alias.
fn multi_word_devanagari_keyword(text: &str) -> Option<TokenKind> {
    let kind = match text {
        "नहीं तो" => TokenKind::Else,       // nahīṁ to (Hindi: "if not / else")
        "के लिए" => TokenKind::For,         // ke liye (Hindi: "for the sake of")
        "सिद्ध करो" => TokenKind::Prove,    // siddha karo (Hindi: "prove!")
        "सिद्ध करा" => TokenKind::Prove,    // siddha karā (Marathi: "prove!")
        "समान्तर प्रति" => TokenKind::Parallel, // samāntara prati (Sanskrit)
        _ => return None,
    };
    Some(kind)
}

/// A `// …` comment recovered from source for later use by tools
/// (currently the formatter). The lexer's main pass drops comments
/// to keep the token stream lean for parsing; this side-channel scan
/// recovers them with their byte spans so a downstream formatter can
/// re-interleave them at the right indent.
#[derive(Clone, Debug, PartialEq)]
pub struct Comment {
    /// The full text of the line including the leading `//`. Trailing
    /// whitespace before the newline is preserved verbatim so that a
    /// careful tool could reproduce the original exactly; the
    /// formatter trims it.
    pub text: String,
    pub span: Span,
}

/// Scan `source` for `//` line comments, returning them in document
/// order. String literals are skipped correctly so `"//"` inside a
/// string is not mistaken for a comment. This is a deliberately
/// separate pass from `lex`: keeping comments off the main token
/// stream avoids polluting every parser site with comment-skipping
/// logic.
pub fn extract_comments(source: &str) -> Vec<Comment> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                // Skip a string literal. Honors `\X` two-byte escapes
                // so that `"\""` isn't terminated by the inner quote.
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == b'"' {
                        i += 1;
                        break;
                    }
                    if bytes[i] == b'\n' {
                        // The real lexer will surface this. Bail out
                        // so we don't claim everything after as
                        // string content.
                        break;
                    }
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                let start = i;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                let text = std::str::from_utf8(&bytes[start..i])
                    .unwrap_or("")
                    .to_string();
                out.push(Comment {
                    text,
                    span: Span::new(start, i),
                });
            }
            _ => i += 1,
        }
    }
    out
}

struct Lexer<'a> {
    source: &'a str,
    bytes: &'a [u8],
    pos: usize,
    tokens: Vec<Token>,
}

impl<'a> Lexer<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            source,
            bytes: source.as_bytes(),
            pos: 0,
            tokens: Vec::new(),
        }
    }

    fn lex(mut self) -> Result<Vec<Token>, Diagnostic> {
        while !self.is_at_end() {
            let start = self.pos;
            let byte = self.advance();

            match byte {
                b' ' | b'\r' | b'\t' | b'\n' => {}
                b'/' if self.match_byte(b'/') => self.skip_line_comment(),
                b'0'..=b'9' => self.lex_number(start)?,
                b'a'..=b'z' | b'A'..=b'Z' | b'_' => self.lex_ident(start),
                // Non-ASCII byte: start of a UTF-8 multi-byte
                // codepoint sequence. Devanagari letters (U+0900
                // – U+097F) and numerals (U+0966 – U+096F) live
                // here. Numerals start `E0 A5 A6..A5 AF` in UTF-8;
                // dispatch them to `lex_devanagari_number`, others
                // to `lex_unicode_ident`. Item #9 — Sanskrit /
                // Hindi / Marathi.
                b if b >= 0x80 => {
                    if b == 0xE0
                        && self.peek() == Some(0xA5)
                        && matches!(self.peek_next(), Some(0xA6..=0xAF))
                    {
                        self.lex_devanagari_number(start)?;
                    } else {
                        self.lex_unicode_ident(start);
                    }
                }
                b'"' => self.lex_string(start)?,
                b'(' => self.push(TokenKind::LParen, start),
                b')' => self.push(TokenKind::RParen, start),
                b'{' => self.push(TokenKind::LBrace, start),
                b'}' => self.push(TokenKind::RBrace, start),
                b'[' => self.push(TokenKind::LBracket, start),
                b']' => self.push(TokenKind::RBracket, start),
                b':' if self.match_byte(b':') => self.push(TokenKind::ColonColon, start),
                b':' => self.push(TokenKind::Colon, start),
                b';' => self.push(TokenKind::Semicolon, start),
                b',' => self.push(TokenKind::Comma, start),
                b'+' => self.push(TokenKind::Plus, start),
                b'-' if self.match_byte(b'>') => self.push(TokenKind::Arrow, start),
                b'-' => self.push(TokenKind::Minus, start),
                b'*' => self.push(TokenKind::Star, start),
                b'/' => self.push(TokenKind::Slash, start),
                b'%' => self.push(TokenKind::Percent, start),
                b'!' if self.match_byte(b'=') => self.push(TokenKind::BangEq, start),
                b'!' => self.push(TokenKind::Bang, start),
                b'=' if self.match_byte(b'=') => self.push(TokenKind::EqEq, start),
                b'=' => self.push(TokenKind::Equal, start),
                b'<' if self.match_byte(b'<') => self.push(TokenKind::LessLess, start),
                b'<' if self.match_byte(b'=') => self.push(TokenKind::LessEq, start),
                b'<' => self.push(TokenKind::Less, start),
                b'>' if self.match_byte(b'>') => self.push(TokenKind::GreaterGreater, start),
                b'>' if self.match_byte(b'=') => self.push(TokenKind::GreaterEq, start),
                b'>' => self.push(TokenKind::Greater, start),
                b'&' if self.match_byte(b'&') => self.push(TokenKind::AndAnd, start),
                b'&' => self.push(TokenKind::Amp, start),
                b'|' if self.match_byte(b'|') => self.push(TokenKind::OrOr, start),
                b'|' => self.push(TokenKind::Pipe, start),
                b'^' => self.push(TokenKind::Caret, start),
                b'.' if self.match_byte(b'.') => self.push(TokenKind::DotDot, start),
                b'.' => self.push(TokenKind::Dot, start),
                other => {
                    return Err(Diagnostic::new(
                        Span::new(start, start + 1),
                        format!("unexpected character '{}'", other as char),
                    ));
                }
            }
        }

        self.tokens.push(Token {
            kind: TokenKind::Eof,
            span: Span::new(self.source.len(), self.source.len()),
        });
        Ok(self.tokens)
    }

    /// Lex a Devanagari integer literal — sequence of digits from
    /// `०१२३४५६७८९` (U+0966 – U+096F). The first lead byte `0xE0`
    /// has already been consumed; the next two bytes are read as
    /// part of the first digit, then any subsequent Devanagari
    /// digits are consumed too. The resulting digit string is
    /// translated to ASCII and parsed via `i128::from_str_radix`.
    /// No suffix / float / radix / underscore support in this
    /// first cut — Devanagari literals are integers only, for
    /// readability of small numbers in source. Item #9 follow-up.
    fn lex_devanagari_number(&mut self, start: usize) -> Result<(), Diagnostic> {
        // Consume the remaining two bytes of the first codepoint
        // (`0xA5` then `0xA6..=0xAF` — already pre-checked at the
        // dispatch site).
        self.advance(); // 0xA5
        self.advance(); // digit byte 0xA6..AF
        // Consume any further Devanagari digits.
        while self.peek() == Some(0xE0)
            && self.peek_next() == Some(0xA5)
            && matches!(
                self.bytes.get(self.pos + 2).copied(),
                Some(0xA6..=0xAF)
            )
        {
            self.advance(); // 0xE0
            self.advance(); // 0xA5
            self.advance(); // digit
        }
        let span = Span::new(start, self.pos);
        let raw = &self.source[start..self.pos];
        let mut ascii_digits = String::with_capacity(raw.chars().count());
        for ch in raw.chars() {
            // Devanagari digit codepoints U+0966..U+096F map to
            // ASCII '0'..'9' by subtracting 0x0966.
            let code = ch as u32;
            ascii_digits.push((b'0' + (code - 0x0966) as u8) as char);
        }
        let value: i128 = ascii_digits.parse().map_err(|_| {
            Diagnostic::new(span, format!("invalid Devanagari integer '{}'", raw))
        })?;
        self.tokens.push(Token {
            kind: TokenKind::Int(value),
            span,
        });
        Ok(())
    }

    fn lex_number(&mut self, start: usize) -> Result<(), Diagnostic> {
        let first = self.bytes[start];
        if first == b'0' && matches!(self.peek(), Some(b'x' | b'X' | b'b' | b'B' | b'o' | b'O')) {
            return self.lex_radix_int(start);
        }

        while matches!(self.peek(), Some(b'0'..=b'9' | b'_')) {
            self.advance();
        }

        let mut is_float = false;

        if self.peek() == Some(b'.') && matches!(self.peek_next(), Some(b'0'..=b'9')) {
            is_float = true;
            self.advance();
            while matches!(self.peek(), Some(b'0'..=b'9' | b'_')) {
                self.advance();
            }
        }

        if matches!(self.peek(), Some(b'e' | b'E')) {
            let exponent_start = self.pos;
            self.advance();
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.advance();
            }
            if matches!(self.peek(), Some(b'0'..=b'9')) {
                is_float = true;
                while matches!(self.peek(), Some(b'0'..=b'9' | b'_')) {
                    self.advance();
                }
            } else {
                return Err(Diagnostic::new(
                    Span::new(exponent_start, self.pos),
                    "expected digits after float exponent",
                ));
            }
        }

        let span = Span::new(start, self.pos);
        let raw = &self.source[start..self.pos];
        let cleaned = strip_underscores(raw);

        if is_float {
            let value = cleaned.parse::<f64>().map_err(|_| {
                Diagnostic::new(span, format!("float literal '{}' cannot be parsed", raw))
            })?;
            if !value.is_finite() {
                return Err(Diagnostic::new(
                    span,
                    format!("float literal '{}' is not finite", raw),
                ));
            }
            self.tokens.push(Token {
                kind: TokenKind::Float(value),
                span,
            });
            return Ok(());
        }

        let value = cleaned.parse::<i128>().map_err(|_| {
            Diagnostic::new(
                span,
                format!("integer literal '{}' does not fit in i128", raw),
            )
        })?;

        self.tokens.push(Token {
            kind: TokenKind::Int(value),
            span,
        });
        Ok(())
    }

    fn lex_radix_int(&mut self, start: usize) -> Result<(), Diagnostic> {
        let prefix = self.advance();
        let (radix, name): (u32, &str) = match prefix {
            b'x' | b'X' => (16, "hex"),
            b'b' | b'B' => (2, "binary"),
            b'o' | b'O' => (8, "octal"),
            _ => unreachable!("called only on valid radix prefixes"),
        };

        let digits_start = self.pos;
        while let Some(byte) = self.peek() {
            if byte == b'_' || is_digit_for_radix(byte, radix) {
                self.advance();
            } else {
                break;
            }
        }

        if self.pos == digits_start {
            return Err(Diagnostic::new(
                Span::new(start, self.pos),
                format!("expected {} digits after '0{}' prefix", name, prefix as char),
            ));
        }

        let span = Span::new(start, self.pos);
        let cleaned = strip_underscores(&self.source[digits_start..self.pos]);
        let value = i128::from_str_radix(&cleaned, radix).map_err(|_| {
            Diagnostic::new(
                span,
                format!(
                    "{} integer literal '{}' does not fit in i128",
                    name,
                    &self.source[start..self.pos]
                ),
            )
        })?;

        self.tokens.push(Token {
            kind: TokenKind::Int(value),
            span,
        });
        Ok(())
    }

    /// Lex an identifier that begins with a non-ASCII codepoint
    /// (e.g. Devanagari letters). Consumes every following byte
    /// that's either an identifier-continuation ASCII character
    /// or any non-ASCII byte (which by validated-UTF-8 source
    /// invariant means it's part of another codepoint). Then
    /// matches the resulting string against the Devanagari
    /// keyword-alias table — if a hit, route to the corresponding
    /// English TokenKind. Otherwise treat as a Unicode identifier
    /// name (`Ident`).
    fn lex_unicode_ident(&mut self, start: usize) {
        while let Some(b) = self.peek() {
            if matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_')
                || b >= 0x80
            {
                self.advance();
            } else {
                break;
            }
        }
        let text = &self.source[start..self.pos];
        let kind = devanagari_keyword(text)
            .unwrap_or_else(|| TokenKind::Ident(text.to_owned()));
        self.tokens.push(Token {
            kind,
            span: Span::new(start, self.pos),
        });
    }

    fn lex_ident(&mut self, start: usize) {
        while matches!(
            self.peek(),
            Some(b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_')
        ) {
            self.advance();
        }

        let text = &self.source[start..self.pos];
        // English keyword table — primary spelling on the
        // left, alias rows below it. Each alias maps to the
        // same TokenKind so the parser doesn't need to know
        // about the alternate spelling. Alias selection is
        // conservative: only word forms that are very
        // unlikely to collide with user-chosen identifiers
        // (variable / param / field names). Common
        // identifier-shaped words like `def`, `function`,
        // `bind`, `mutable`, `constant`, `otherwise` are
        // deliberately NOT added — they'd silently break
        // existing user code that uses them as names. Once
        // per-file language purity (TODO item) ships, that
        // gate can declare safe-vs-collision contexts and
        // unlock the broader set.
        let kind = match text {
            "fn" => TokenKind::Fn,
            "pure" => TokenKind::Pure,
            "extern" => TokenKind::Extern,
            "parallel" => TokenKind::Parallel,
            "reduce" => TokenKind::Reduce,
            "with" => TokenKind::With,
            "task" => TokenKind::Task,
            "join" => TokenKind::Join,
            // Note: `min` / `max` are NOT global reserved
            // keywords — they're context-sensitive
            // identifiers used by `reduce X with min;`
            // and the `min(a,b)` / `max(a,b)` intrinsics.
            // Users can declare struct fields, locals,
            // and other names called `min`/`max` without
            // collision.
            // Local binding: `let` is the idiomatic form;
            // `assign` reads naturally for newcomers approaching
            // from a Python / pseudo-code background. Closure
            // #255 — pure surface alias, identical AST.
            "let" | "assign" => TokenKind::Let,
            // Function exit: `return` and three English-natural
            // aliases. `give` is the verb form ("give the
            // value"); `give_back` is the snake-case multi-word
            // form; the two-word `give back` is folded later in
            // a post-lex pass (`merge_multi_word_give_back`) so
            // the surface accepts whichever spelling the writer
            // prefers. Closure #255.
            "return" | "give" | "give_back" => TokenKind::Return,
            "if" => TokenKind::If,
            "else" => TokenKind::Else,
            "while" => TokenKind::While,
            "break" => TokenKind::Break,
            "continue" => TokenKind::Continue,
            "mut" => TokenKind::Mut,
            "for" => TokenKind::For,
            "in" => TokenKind::In,
            "ref" => TokenKind::Ref,
            "from" => TokenKind::From,
            "to" => TokenKind::To,
            // Data shape: `struct` / `record`.
            "struct" | "record" => TokenKind::Struct,
            "enum" => TokenKind::Enum,
            "match" => TokenKind::Match,
            "then" => TokenKind::Then,
            // Interface: `interface` / `trait` (Rust-style).
            "interface" | "trait" => TokenKind::Interface,
            // Implementation: `implement` / `impl` (Rust-style).
            "implement" | "impl" => TokenKind::Implement,
            // Module declaration: `module` (canonical) / `mod`
            // (Rust-shorthand alias). Closure #242.
            "module" | "mod" => TokenKind::Module,
            // Visibility modifier: `pub` (canonical, Rust-style)
            // / `public` (alias for newcomers). Makes a
            // module-scoped item visible from outside the
            // module. Closure #242.
            "pub" | "public" => TokenKind::Pub,
            "where" => TokenKind::Where,
            "is" => TokenKind::Is,
            "const" => TokenKind::Const,
            "type" => TokenKind::Type,
            "methods" => TokenKind::Methods,
            "intent" => TokenKind::Intent,
            "use" => TokenKind::Use,
            "requires" => TokenKind::Requires,
            "ensures" => TokenKind::Ensures,
            "invariant" => TokenKind::Invariant,
            "assert" => TokenKind::Assert,
            "prove" => TokenKind::Prove,
            // Output: `print` (legacy / C-Python heritage) /
            // `write` (matches `write(stdout, ...)` style).
            // `write` is preferred in new code; both currently
            // accepted.
            "print" | "write" => TokenKind::Print,
            "try" => TokenKind::Try,
            "len" => TokenKind::Len,
            "as" => TokenKind::As,
            "true" => TokenKind::True,
            "false" => TokenKind::False,
            // Return-type arrow word forms: `returns` /
            // `yields` mean the same as `->`. Reads
            // naturally: `fn f(x: i64) yields i64 { ... }`.
            // Both words are uncommon as identifiers.
            "returns" | "yields" => TokenKind::Arrow,
            "i8" => TokenKind::I8,
            "i16" => TokenKind::I16,
            "i32" => TokenKind::I32,
            "i64" => TokenKind::I64,
            "u8" => TokenKind::U8,
            "u16" => TokenKind::U16,
            "u32" => TokenKind::U32,
            "u64" => TokenKind::U64,
            "f32" => TokenKind::F32,
            "f64" => TokenKind::F64,
            "bool" => TokenKind::Bool,
            "Vec" => TokenKind::Vec,
            _ => TokenKind::Ident(text.to_owned()),
        };

        self.tokens.push(Token {
            kind,
            span: Span::new(start, self.pos),
        });
    }

    fn lex_string(&mut self, start: usize) -> Result<(), Diagnostic> {
        let mut value = String::new();

        while let Some(byte) = self.peek() {
            match byte {
                b'"' => {
                    self.advance();
                    self.tokens.push(Token {
                        kind: TokenKind::Str(value),
                        span: Span::new(start, self.pos),
                    });
                    return Ok(());
                }
                b'\\' => {
                    self.advance();
                    let Some(escaped) = self.peek() else {
                        break;
                    };
                    self.advance();
                    match escaped {
                        b'"' => value.push('"'),
                        b'\\' => value.push('\\'),
                        b'n' => value.push('\n'),
                        b't' => value.push('\t'),
                        b'r' => value.push('\r'),
                        b'0' => value.push('\0'),
                        other => {
                            return Err(Diagnostic::new(
                                Span::new(self.pos.saturating_sub(2), self.pos),
                                format!("unknown escape sequence '\\{}'", other as char),
                            ));
                        }
                    }
                }
                b'\n' => {
                    return Err(Diagnostic::new(
                        Span::new(start, self.pos),
                        "string literal cannot span lines",
                    ));
                }
                _ => {
                    let char_start = self.pos;
                    let ch = self
                        .next_char()
                        .ok_or_else(|| Diagnostic::new(
                            Span::new(char_start, self.pos),
                            "invalid character in string literal",
                        ))?;
                    value.push(ch);
                }
            }
        }

        Err(Diagnostic::new(
            Span::new(start, self.pos),
            "unterminated string literal",
        ))
    }

    fn skip_line_comment(&mut self) {
        while !matches!(self.peek(), None | Some(b'\n')) {
            self.advance();
        }
    }

    fn is_at_end(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn peek_next(&self) -> Option<u8> {
        self.bytes.get(self.pos + 1).copied()
    }

    fn advance(&mut self) -> u8 {
        let byte = self.bytes[self.pos];
        self.pos += 1;
        byte
    }

    fn next_char(&mut self) -> Option<char> {
        let ch = self.source[self.pos..].chars().next()?;
        self.pos += ch.len_utf8();
        Some(ch)
    }

    fn match_byte(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn push(&mut self, kind: TokenKind, start: usize) {
        self.tokens.push(Token {
            kind,
            span: Span::new(start, self.pos),
        });
    }
}

fn strip_underscores(text: &str) -> String {
    text.chars().filter(|ch| *ch != '_').collect()
}

fn is_digit_for_radix(byte: u8, radix: u32) -> bool {
    match radix {
        2 => matches!(byte, b'0' | b'1'),
        8 => matches!(byte, b'0'..=b'7'),
        16 => byte.is_ascii_hexdigit(),
        10 => byte.is_ascii_digit(),
        _ => false,
    }
}
