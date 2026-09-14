//! Port of the tl-parser recursive-descent stage (`tl_parse_lex` and the
//! `parse_*` functions of `td/generate/tl-parser/tl-parser.c`): turns the
//! token stream into a parse tree whose nodes carry source-text ranges.
//!
//! The C implementation backtracks by value-copying the whole lexer state
//! (`save_parse`/`load_parse`) and discarding the partial tree on failure;
//! this port does the same via [`Lexer::save_state`]/[`Lexer::load_state`]
//! (failed subtrees are simply dropped). The macro shapes translate as:
//!
//! - `PARSE_INIT` → `save_state()` + a fresh node capturing line info;
//! - `PARSE_FAIL` → `load_state(save)` + `None`;
//! - `PARSE_OK` → `Some(node)`;
//! - `PARSE_TRY_PES(x)` → `Some` child required, else fail;
//! - `PARSE_TRY_OPT(x)` → on `Some`, add the child and commit (`Some(T)`);
//! - `PARSE_TRY(x)` → keep the `Option`;
//! - `PARSE_ADD(_type)` → a child node of the given kind;
//! - `EXPECT(s)` → `TreeParser::expect`;
//! - `LEX_CHAR(c)` → `TreeParser::lex_char`.

use crate::lexer::{LexType, Lexer};

/// Parse-tree node kinds, mirroring `enum tree_type` in tl-parser.h.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeType {
    TlProgram,
    FunDeclarations,
    ConstrDeclarations,
    Declaration,
    CombinatorDecl,
    Equals,
    PartialAppDecl,
    FinalDecl,
    FullCombinatorId,
    OptArgs,
    Args,
    Args1,
    Args2,
    Args3,
    Args4,
    BoxedTypeIdent,
    Subexpr,
    PartialCombAppDecl,
    PartialTypeAppDecl,
    FinalNew,
    FinalFinal,
    FinalEmpty,
    VarIdent,
    VarIdentOpt,
    Multiplicity,
    TypeTerm,
    Term,
    Percent,
    ResultType,
    Expr,
    NatTerm,
    CombinatorId,
    NatConst,
    TypeIdent,
    BuiltinCombinatorDecl,
    Exclam,
    OptionalArgDef,
}

/// One parse-tree node: `struct tree` with the token text kept as a byte
/// range into the source (`text`/`len`), plus its lexer `flags`.
#[derive(Debug, Clone)]
pub struct TreeNode {
    pub ty: TreeType,
    pub text: Option<std::ops::Range<usize>>,
    pub flags: i32,
    pub lex_line: i32,
    pub lex_line_pos: i32,
    pub children: Vec<TreeNode>,
}

impl TreeNode {
    fn new(ty: TreeType, line: i32, line_pos: i32) -> TreeNode {
        TreeNode {
            ty,
            text: None,
            flags: 0,
            lex_line: line,
            lex_line_pos: line_pos,
            children: Vec::new(),
        }
    }

    /// Takes the current token's text range and flags (`T->text = lex.ptr`).
    fn set_token(&mut self, lexer: &Lexer) {
        self.text = lexer.lexeme().map(|l| l.as_range());
        self.flags = lexer.lex.flags;
    }

    /// The node's text bytes from the source, empty when unset.
    pub fn text_of<'a>(&self, src: &'a [u8]) -> &'a [u8] {
        self.text.as_ref().map(|r| &src[r.clone()]).unwrap_or(&[])
    }
}

/// Mirrors `tl_parse_lex`: bootstraps the first token and parses the whole
/// program, returning `None` on a lexer error or parse failure.
pub fn tl_parse_lex(lexer: &mut Lexer) -> Option<TreeNode> {
    // `load_parse` bootstrap: `Lexer::new` already set `curch` to the first
    // byte, so only the token pre-fetch remains.
    if lexer.lex.ty == LexType::None {
        lexer.parse_lex();
    }
    if lexer.lex.ty == LexType::Error {
        return None;
    }
    TreeParser { lex: lexer }.parse_program()
}

struct TreeParser<'a> {
    lex: &'a mut Lexer,
}

impl TreeParser<'_> {
    /// `EXPECT(s)` — `true` on success (token consumed), `false` on failure.
    fn expect(&mut self, s: &str) -> bool {
        let ok = match self.lex.lexeme() {
            Some(l) => &self.lex.text()[l.as_range()] == s.as_bytes(),
            None => false,
        };
        if !ok {
            self.lex.error(&format!("Expected {s}"));
            false
        } else {
            self.lex.parse_lex();
            true
        }
    }

    /// `LEX_CHAR(c)`.
    fn lex_char(&self, c: u8) -> bool {
        self.lex.lex.ty == LexType::Char
            && self
                .lex
                .lexeme()
                .is_some_and(|l| self.lex.text()[l.as_range()][0] == c)
    }

    fn node(&self, ty: TreeType) -> TreeNode {
        TreeNode::new(ty, self.lex.line(), self.lex.line_pos())
    }

    fn fail(&mut self, save: crate::lexer::ParseState) {
        self.lex.load_state(save);
    }

    fn parse_boxed_type_ident(&mut self) -> Option<TreeNode> {
        let save = self.lex.save_state();
        let mut t = self.node(TreeType::BoxedTypeIdent);
        if self.lex.lex.ty != LexType::UcIdent {
            self.lex.error("Can not parse boxed type");
            self.fail(save);
            None
        } else {
            t.set_token(self.lex);
            self.lex.parse_lex();
            Some(t)
        }
    }

    fn parse_full_combinator_id(&mut self) -> Option<TreeNode> {
        let save = self.lex.save_state();
        let mut t = self.node(TreeType::FullCombinatorId);
        if self.lex.lex.ty == LexType::LcIdent || self.lex_char(b'_') {
            t.set_token(self.lex);
            self.lex.parse_lex();
            Some(t)
        } else {
            self.lex.error("Can not parse full combinator id");
            self.fail(save);
            None
        }
    }

    fn parse_combinator_id(&mut self) -> Option<TreeNode> {
        let save = self.lex.save_state();
        let mut t = self.node(TreeType::CombinatorId);
        if self.lex.lex.ty == LexType::LcIdent && self.lex.lex.flags & 2 == 0 {
            t.set_token(self.lex);
            self.lex.parse_lex();
            Some(t)
        } else {
            self.lex.error("Can not parse combinator id");
            self.fail(save);
            None
        }
    }

    fn parse_var_ident(&mut self) -> Option<TreeNode> {
        let save = self.lex.save_state();
        let mut t = self.node(TreeType::VarIdent);
        let ty = self.lex.lex.ty;
        if (ty == LexType::LcIdent || ty == LexType::UcIdent) && self.lex.lex.flags & 3 == 0 {
            t.set_token(self.lex);
            self.lex.parse_lex();
            Some(t)
        } else {
            self.lex.error("Can not parse var ident");
            self.fail(save);
            None
        }
    }

    fn parse_var_ident_opt(&mut self) -> Option<TreeNode> {
        let save = self.lex.save_state();
        let mut t = self.node(TreeType::VarIdentOpt);
        let ty = self.lex.lex.ty;
        let is_ident =
            (ty == LexType::LcIdent || ty == LexType::UcIdent) && self.lex.lex.flags & 3 == 0;
        if is_ident || self.lex_char(b'_') {
            t.set_token(self.lex);
            self.lex.parse_lex();
            Some(t)
        } else {
            self.lex.error("Can not parse var ident opt");
            self.fail(save);
            None
        }
    }

    fn parse_nat_const(&mut self) -> Option<TreeNode> {
        let save = self.lex.save_state();
        let mut t = self.node(TreeType::NatConst);
        if self.lex.lex.ty == LexType::Num {
            t.set_token(self.lex);
            self.lex.parse_lex();
            Some(t)
        } else {
            self.lex.error("Can not parse nat const");
            self.fail(save);
            None
        }
    }

    fn parse_type_ident(&mut self) -> Option<TreeNode> {
        let save = self.lex.save_state();
        let mut t = self.node(TreeType::TypeIdent);
        let ty = self.lex.lex.ty;
        let is_ident =
            (ty == LexType::UcIdent || ty == LexType::LcIdent) && self.lex.lex.flags & 2 == 0;
        if is_ident || self.lex_char(b'#') {
            t.set_token(self.lex);
            self.lex.parse_lex();
            Some(t)
        } else {
            self.lex.error("Can not parse type ident");
            self.fail(save);
            None
        }
    }

    fn parse_term(&mut self) -> Option<TreeNode> {
        let save = self.lex.save_state();
        let mut t = self.node(TreeType::Term);
        while self.lex_char(b'%') {
            if !self.expect("%") {
                self.fail(save);
                return None;
            }
            t.children.push(self.node(TreeType::Percent));
        }
        if self.lex_char(b'(') {
            if !self.expect("(") {
                self.fail(save);
                return None;
            }
            match self.parse_expr() {
                Some(s) => t.children.push(s),
                None => {
                    self.fail(save);
                    return None;
                }
            }
            if !self.expect(")") {
                self.fail(save);
                return None;
            }
            return Some(t);
        }
        let s = self.parse_type_ident();
        if let Some(s) = s {
            t.children.push(s);
            if self.lex_char(b'<') {
                if !self.expect("<") {
                    self.fail(save);
                    return None;
                }
                loop {
                    match self.parse_expr() {
                        Some(s) => t.children.push(s),
                        None => {
                            self.fail(save);
                            return None;
                        }
                    }
                    if self.lex_char(b'>') {
                        break;
                    }
                    if !self.expect(",") {
                        self.fail(save);
                        return None;
                    }
                }
                if !self.expect(">") {
                    self.fail(save);
                    return None;
                }
            }
            return Some(t);
        }
        // `PARSE_TRY_OPT(parse_type_ident)`: just failed above, fails again —
        // kept for shape parity with the C source.
        if self.parse_type_ident().is_some() {
            unreachable!("parse_type_ident is deterministic")
        }
        if let Some(s) = self.parse_var_ident() {
            t.children.push(s);
            return Some(t);
        }
        if let Some(s) = self.parse_nat_const() {
            t.children.push(s);
            return Some(t);
        }
        self.fail(save);
        None
    }

    fn parse_nat_term(&mut self) -> Option<TreeNode> {
        let save = self.lex.save_state();
        let mut t = self.node(TreeType::NatTerm);
        match self.parse_term() {
            Some(s) => {
                t.children.push(s);
                Some(t)
            }
            None => {
                self.fail(save);
                None
            }
        }
    }

    fn parse_subexpr(&mut self) -> Option<TreeNode> {
        let save = self.lex.save_state();
        let mut t = self.node(TreeType::Subexpr);
        let mut was_term = false;
        let mut cc = 0;
        loop {
            let s = self.parse_nat_const();
            if let Some(s) = s {
                t.children.push(s);
            } else if !was_term {
                was_term = true;
                match self.parse_term() {
                    Some(s) => t.children.push(s),
                    None => break,
                }
            }
            cc += 1;
            if !self.lex_char(b'+') {
                break;
            }
            if !self.expect("+") {
                self.fail(save);
                return None;
            }
        }
        if cc == 0 {
            self.fail(save);
            None
        } else {
            Some(t)
        }
    }

    fn parse_expr(&mut self) -> Option<TreeNode> {
        let save = self.lex.save_state();
        let mut t = self.node(TreeType::Expr);
        let mut cc = 0;
        loop {
            match self.parse_subexpr() {
                Some(s) => {
                    t.children.push(s);
                    cc += 1;
                }
                None => {
                    if cc < 1 {
                        self.fail(save);
                        return None;
                    }
                    return Some(t);
                }
            }
        }
    }

    fn parse_final_empty(&mut self) -> Option<TreeNode> {
        let save = self.lex.save_state();
        let mut t = self.node(TreeType::FinalEmpty);
        if !self.expect("Empty") {
            self.fail(save);
            return None;
        }
        match self.parse_boxed_type_ident() {
            Some(s) => {
                t.children.push(s);
                Some(t)
            }
            None => {
                self.fail(save);
                None
            }
        }
    }

    fn parse_final_new(&mut self) -> Option<TreeNode> {
        let save = self.lex.save_state();
        let mut t = self.node(TreeType::FinalNew);
        if !self.expect("New") {
            self.fail(save);
            return None;
        }
        match self.parse_boxed_type_ident() {
            Some(s) => {
                t.children.push(s);
                Some(t)
            }
            None => {
                self.fail(save);
                None
            }
        }
    }

    fn parse_final_final(&mut self) -> Option<TreeNode> {
        let save = self.lex.save_state();
        let mut t = self.node(TreeType::FinalFinal);
        if !self.expect("Final") {
            self.fail(save);
            return None;
        }
        match self.parse_boxed_type_ident() {
            Some(s) => {
                t.children.push(s);
                Some(t)
            }
            None => {
                self.fail(save);
                None
            }
        }
    }

    fn parse_partial_comb_app_decl(&mut self) -> Option<TreeNode> {
        let save = self.lex.save_state();
        let mut t = self.node(TreeType::PartialCombAppDecl);
        match self.parse_combinator_id() {
            Some(s) => t.children.push(s),
            None => {
                self.fail(save);
                return None;
            }
        }
        // Deliberate divergence from the C tl-parser (see
        // `parse_partial_type_app_decl`): the subexpr loop must make
        // forward progress or the parse fails, instead of hanging forever.
        let mut last_pos = self.lex.pos();
        loop {
            match self.parse_subexpr() {
                Some(s) => t.children.push(s),
                None => {
                    self.fail(save);
                    return None;
                }
            }
            let pos = self.lex.pos();
            if pos == last_pos {
                self.lex
                    .error("Can not parse partial combinator application");
                self.fail(save);
                return None;
            }
            last_pos = pos;
            if self.lex_char(b';') {
                break;
            }
        }
        Some(t)
    }

    fn parse_partial_type_app_decl(&mut self) -> Option<TreeNode> {
        let save = self.lex.save_state();
        let mut t = self.node(TreeType::PartialTypeAppDecl);
        match self.parse_boxed_type_ident() {
            Some(s) => t.children.push(s),
            None => {
                self.fail(save);
                return None;
            }
        }
        if self.lex_char(b'<') {
            if !self.expect("<") {
                self.fail(save);
                return None;
            }
            loop {
                match self.parse_expr() {
                    Some(s) => t.children.push(s),
                    None => {
                        self.fail(save);
                        return None;
                    }
                }
                if self.lex_char(b'>') {
                    break;
                }
                if !self.expect(",") {
                    self.fail(save);
                    return None;
                }
            }
            if !self.expect(">") {
                self.fail(save);
                return None;
            }
            Some(t)
        } else {
            // Deliberate divergence from the C tl-parser (matching
            // `parse_partial_comb_app_decl`): require forward progress.
            // The C `while (1) { parse_subexpr; if ';' break; }` loop spins
            // forever appending tree nodes when the final token is a stale
            // zero-length identifier — found by the S78 fuzz week on the
            // 1-byte input "J" (the C parser hangs identically).
            let mut last_pos = self.lex.pos();
            loop {
                match self.parse_subexpr() {
                    Some(s) => t.children.push(s),
                    None => {
                        self.fail(save);
                        return None;
                    }
                }
                let pos = self.lex.pos();
                if pos == last_pos {
                    self.lex.error("Can not parse partial type application");
                    self.fail(save);
                    return None;
                }
                last_pos = pos;
                if self.lex_char(b';') {
                    break;
                }
            }
            Some(t)
        }
    }

    fn parse_multiplicity(&mut self) -> Option<TreeNode> {
        let save = self.lex.save_state();
        let mut t = self.node(TreeType::Multiplicity);
        match self.parse_nat_term() {
            Some(s) => {
                t.children.push(s);
                Some(t)
            }
            None => {
                self.fail(save);
                None
            }
        }
    }

    fn parse_type_term(&mut self) -> Option<TreeNode> {
        let save = self.lex.save_state();
        let mut t = self.node(TreeType::TypeTerm);
        match self.parse_term() {
            Some(s) => {
                t.children.push(s);
                Some(t)
            }
            None => {
                self.fail(save);
                None
            }
        }
    }

    fn parse_optional_arg_def(&mut self) -> Option<TreeNode> {
        let save = self.lex.save_state();
        let mut t = self.node(TreeType::OptionalArgDef);
        match self.parse_var_ident() {
            Some(s) => t.children.push(s),
            None => {
                self.fail(save);
                return None;
            }
        }
        if !self.expect(".") {
            self.fail(save);
            return None;
        }
        match self.parse_nat_const() {
            Some(s) => t.children.push(s),
            None => {
                self.fail(save);
                return None;
            }
        }
        if !self.expect("?") {
            self.fail(save);
            return None;
        }
        Some(t)
    }

    fn parse_args4(&mut self) -> Option<TreeNode> {
        let save = self.lex.save_state();
        let mut t = self.node(TreeType::Args4);
        let so = self.lex.save_state();
        if let Some(s) = self.parse_optional_arg_def() {
            t.children.push(s);
        } else {
            self.lex.load_state(so);
        }
        if self.lex_char(b'!') {
            t.children.push(self.node(TreeType::Exclam));
            if !self.expect("!") {
                self.fail(save);
                return None;
            }
        }
        match self.parse_type_term() {
            Some(s) => {
                t.children.push(s);
                Some(t)
            }
            None => {
                self.fail(save);
                None
            }
        }
    }

    fn parse_args3(&mut self) -> Option<TreeNode> {
        let save = self.lex.save_state();
        let mut t = self.node(TreeType::Args3);
        match self.parse_var_ident_opt() {
            Some(s) => t.children.push(s),
            None => {
                self.fail(save);
                return None;
            }
        }
        if !self.expect(":") {
            self.fail(save);
            return None;
        }
        let so = self.lex.save_state();
        if let Some(s) = self.parse_optional_arg_def() {
            t.children.push(s);
        } else {
            self.lex.load_state(so);
        }
        if self.lex_char(b'!') {
            t.children.push(self.node(TreeType::Exclam));
            if !self.expect("!") {
                self.fail(save);
                return None;
            }
        }
        match self.parse_type_term() {
            Some(s) => {
                t.children.push(s);
                Some(t)
            }
            None => {
                self.fail(save);
                None
            }
        }
    }

    fn parse_args2(&mut self) -> Option<TreeNode> {
        let save = self.lex.save_state();
        let mut t = self.node(TreeType::Args2);
        // `var:` is only an args2 when a ':' follows; otherwise rewind to the
        // entry state (`load_parse (save)` in C).
        let var = self.parse_var_ident_opt();
        let colon = self.lex_char(b':');
        if let (Some(var), true) = (var, colon) {
            t.children.push(var);
            if !self.expect(":") {
                self.fail(save);
                return None;
            }
        } else {
            self.fail(save);
        }
        let so = self.lex.save_state();
        if let Some(s) = self.parse_optional_arg_def() {
            t.children.push(s);
        } else {
            self.lex.load_state(so);
        }
        // A multiplicity only belongs to this args2 when followed by '*'.
        let save2 = self.lex.save_state();
        let mult = self.parse_multiplicity();
        let star = self.lex_char(b'*');
        if let (Some(mult), true) = (mult, star) {
            t.children.push(mult);
            if !self.expect("*") {
                self.fail(save);
                return None;
            }
        } else {
            self.lex.load_state(save2);
        }
        if !self.expect("[") {
            self.fail(save);
            return None;
        }
        loop {
            if self.lex_char(b']') {
                break;
            }
            match self.parse_args() {
                Some(s) => t.children.push(s),
                None => {
                    self.fail(save);
                    return None;
                }
            }
        }
        if !self.expect("]") {
            self.fail(save);
            return None;
        }
        Some(t)
    }

    fn parse_args1(&mut self) -> Option<TreeNode> {
        let save = self.lex.save_state();
        let mut t = self.node(TreeType::Args1);
        if !self.expect("(") {
            self.fail(save);
            return None;
        }
        loop {
            // As in parse_opt_args: the var-ident nodes are children.
            match self.parse_var_ident_opt() {
                Some(s) => t.children.push(s),
                None => {
                    self.fail(save);
                    return None;
                }
            }
            if self.lex_char(b':') {
                break;
            }
        }
        if !self.expect(":") {
            self.fail(save);
            return None;
        }
        let so = self.lex.save_state();
        if let Some(s) = self.parse_optional_arg_def() {
            t.children.push(s);
        } else {
            self.lex.load_state(so);
        }
        if self.lex_char(b'!') {
            t.children.push(self.node(TreeType::Exclam));
            if !self.expect("!") {
                self.fail(save);
                return None;
            }
        }
        match self.parse_type_term() {
            Some(s) => t.children.push(s),
            None => {
                self.fail(save);
                return None;
            }
        }
        if !self.expect(")") {
            self.fail(save);
            return None;
        }
        Some(t)
    }

    fn parse_args(&mut self) -> Option<TreeNode> {
        let save = self.lex.save_state();
        let mut t = self.node(TreeType::Args);
        if let Some(s) = self.parse_args1() {
            t.children.push(s);
            return Some(t);
        }
        if let Some(s) = self.parse_args2() {
            t.children.push(s);
            return Some(t);
        }
        if let Some(s) = self.parse_args3() {
            t.children.push(s);
            return Some(t);
        }
        if let Some(s) = self.parse_args4() {
            t.children.push(s);
            return Some(t);
        }
        self.fail(save);
        None
    }

    fn parse_opt_args(&mut self) -> Option<TreeNode> {
        let save = self.lex.save_state();
        let mut t = self.node(TreeType::OptArgs);
        loop {
            // The var-ident nodes are children (PARSE_TRY_PES adds them);
            // tl_parse_opt_args reads them back by index.
            match self.parse_var_ident() {
                Some(s) => t.children.push(s),
                None => {
                    self.fail(save);
                    return None;
                }
            }
            if self.lex_char(b':') {
                break;
            }
        }
        if !self.expect(":") {
            self.fail(save);
            return None;
        }
        match self.parse_type_term() {
            Some(s) => {
                t.children.push(s);
                Some(t)
            }
            None => {
                self.fail(save);
                None
            }
        }
    }

    fn parse_final_decl(&mut self) -> Option<TreeNode> {
        let save = self.lex.save_state();
        let mut t = self.node(TreeType::FinalDecl);
        if let Some(s) = self.parse_final_new() {
            t.children.push(s);
            return Some(t);
        }
        if let Some(s) = self.parse_final_final() {
            t.children.push(s);
            return Some(t);
        }
        if let Some(s) = self.parse_final_empty() {
            t.children.push(s);
            return Some(t);
        }
        self.fail(save);
        None
    }

    fn parse_partial_app_decl(&mut self) -> Option<TreeNode> {
        let save = self.lex.save_state();
        let mut t = self.node(TreeType::PartialAppDecl);
        if let Some(s) = self.parse_partial_type_app_decl() {
            t.children.push(s);
            return Some(t);
        }
        if let Some(s) = self.parse_partial_comb_app_decl() {
            t.children.push(s);
            return Some(t);
        }
        self.fail(save);
        None
    }

    fn parse_result_type(&mut self) -> Option<TreeNode> {
        let save = self.lex.save_state();
        let mut t = self.node(TreeType::ResultType);
        match self.parse_boxed_type_ident() {
            Some(s) => t.children.push(s),
            None => {
                self.fail(save);
                return None;
            }
        }
        if self.lex_char(b'<') {
            if !self.expect("<") {
                self.fail(save);
                return None;
            }
            loop {
                match self.parse_expr() {
                    Some(s) => t.children.push(s),
                    None => {
                        self.fail(save);
                        return None;
                    }
                }
                if self.lex_char(b'>') {
                    break;
                }
                if !self.expect(",") {
                    self.fail(save);
                    return None;
                }
            }
            if !self.expect(">") {
                self.fail(save);
                return None;
            }
            Some(t)
        } else {
            loop {
                if self.lex_char(b';') {
                    return Some(t);
                }
                match self.parse_subexpr() {
                    Some(s) => t.children.push(s),
                    None => {
                        self.fail(save);
                        return None;
                    }
                }
            }
        }
    }

    fn parse_combinator_decl(&mut self) -> Option<TreeNode> {
        let save = self.lex.save_state();
        let mut t = self.node(TreeType::CombinatorDecl);
        match self.parse_full_combinator_id() {
            Some(s) => t.children.push(s),
            None => {
                self.fail(save);
                return None;
            }
        }
        loop {
            if self.lex_char(b'{') {
                self.lex.parse_lex();
                match self.parse_opt_args() {
                    Some(s) => t.children.push(s),
                    None => {
                        self.fail(save);
                        return None;
                    }
                }
                if !self.expect("}") {
                    self.fail(save);
                    return None;
                }
            } else {
                break;
            }
        }
        loop {
            if self.lex_char(b'=') {
                break;
            }
            match self.parse_args() {
                Some(s) => t.children.push(s),
                None => {
                    self.fail(save);
                    return None;
                }
            }
        }
        if !self.expect("=") {
            self.fail(save);
            return None;
        }
        t.children.push(self.node(TreeType::Equals));
        match self.parse_result_type() {
            Some(s) => {
                t.children.push(s);
                Some(t)
            }
            None => {
                self.fail(save);
                None
            }
        }
    }

    fn parse_builtin_combinator_decl(&mut self) -> Option<TreeNode> {
        let save = self.lex.save_state();
        let mut t = self.node(TreeType::BuiltinCombinatorDecl);
        match self.parse_full_combinator_id() {
            Some(s) => t.children.push(s),
            None => {
                self.fail(save);
                return None;
            }
        }
        if !self.expect("?") {
            self.fail(save);
            return None;
        }
        if !self.expect("=") {
            self.fail(save);
            return None;
        }
        match self.parse_boxed_type_ident() {
            Some(s) => {
                t.children.push(s);
                Some(t)
            }
            None => {
                self.fail(save);
                None
            }
        }
    }

    fn parse_declaration(&mut self) -> Option<TreeNode> {
        let save = self.lex.save_state();
        let mut t = self.node(TreeType::Declaration);
        if let Some(s) = self.parse_combinator_decl() {
            t.children.push(s);
            return Some(t);
        }
        if let Some(s) = self.parse_partial_app_decl() {
            t.children.push(s);
            return Some(t);
        }
        if let Some(s) = self.parse_final_decl() {
            t.children.push(s);
            return Some(t);
        }
        if let Some(s) = self.parse_builtin_combinator_decl() {
            t.children.push(s);
            return Some(t);
        }
        self.fail(save);
        None
    }

    fn parse_constr_declarations(&mut self) -> Option<TreeNode> {
        let save = self.lex.save_state();
        let mut t = self.node(TreeType::ConstrDeclarations);
        if self.lex.lex.ty == LexType::TripleMinus || self.lex.lex.ty == LexType::Eof {
            return Some(t);
        }
        loop {
            match self.parse_declaration() {
                Some(s) => t.children.push(s),
                None => {
                    self.fail(save);
                    return None;
                }
            }
            if !self.expect(";") {
                self.fail(save);
                return None;
            }
            if self.lex.lex.ty == LexType::Eof || self.lex.lex.ty == LexType::TripleMinus {
                return Some(t);
            }
        }
    }

    fn parse_fun_declarations(&mut self) -> Option<TreeNode> {
        let save = self.lex.save_state();
        let mut t = self.node(TreeType::FunDeclarations);
        if self.lex.lex.ty == LexType::TripleMinus || self.lex.lex.ty == LexType::Eof {
            return Some(t);
        }
        loop {
            match self.parse_declaration() {
                Some(s) => t.children.push(s),
                None => {
                    self.fail(save);
                    return None;
                }
            }
            if !self.expect(";") {
                self.fail(save);
                return None;
            }
            if self.lex.lex.ty == LexType::Eof || self.lex.lex.ty == LexType::TripleMinus {
                return Some(t);
            }
        }
    }

    fn parse_program(&mut self) -> Option<TreeNode> {
        let save = self.lex.save_state();
        let mut t = self.node(TreeType::TlProgram);
        loop {
            match self.parse_constr_declarations() {
                Some(s) => t.children.push(s),
                None => {
                    self.fail(save);
                    return None;
                }
            }
            if self.lex.lex.ty == LexType::Eof {
                return Some(t);
            }
            if self.lex.lex.ty == LexType::Error
                || !self.expect("---")
                || !self.expect("functions")
                || !self.expect("---")
            {
                self.fail(save);
                return None;
            }
            match self.parse_fun_declarations() {
                Some(s) => t.children.push(s),
                None => {
                    self.fail(save);
                    return None;
                }
            }
            if self.lex.lex.ty == LexType::Eof {
                return Some(t);
            }
            if self.lex.lex.ty == LexType::Error
                || !self.expect("---")
                || !self.expect("types")
                || !self.expect("---")
            {
                self.fail(save);
                return None;
            }
        }
    }
}
