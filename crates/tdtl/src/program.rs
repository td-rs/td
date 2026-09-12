//! Port of the tl-parser program-building stage (`tl_parse` and its helpers
//! in `td/generate/tl-parser/tl-parser.c`): walks the parse tree into the
//! scheme program — types, constructors, functions, and their combinator
//! argument trees — including constructor-id assignment via CRC32 of the
//! canonical combinator string (`tl_count_combinator_name`).
//!
//! The C implementation keeps everything in globals and raw pointers; this
//! port uses arenas indexed by `u32` ids. Pointer identity becomes id
//! identity: a variable is represented by the id of its declaration field
//! node (`act_var` nodes carry that id in [`CTData::Var`]), and the
//! unification maps (`var_value` treaps in C) are `BTreeMap`s keyed by those
//! field-node ids.
//!
//! The C treaps are plain BSTs keyed by `strcmp`, so their in-order walks
//! (used for `tl_type_check`'s side effects and for output ordering) are
//! iteration in byte-sorted id order — `BTreeMap<String, _>` iteration
//! matches exactly (scheme ids are ASCII).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::crc32::compute_crc32;
use crate::lexer::Lexer;
use crate::parse_tree::{TreeNode, TreeType, tl_parse_lex};

pub type TypeId = usize;
pub type CtorId = usize;
pub type CTId = usize;
pub type VarId = usize;

/// `enum combinator_tree_action`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    Var,
    Field,
    Plus,
    Type,
    NatConst,
    Array,
    QuestionMark,
    Union,
    Arg,
    OptField,
}

/// `enum combinator_tree_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CType {
    Num,
    NumValue,
    Type,
    ListItem,
    List,
}

/// The polymorphic `data` pointer of `struct tl_combinator_tree`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CTData {
    /// Null.
    None,
    /// `act_type` without `flags & 2`: the pointed-to type.
    Type(TypeId),
    /// `act_field`: the field name (null for anonymous args); also the
    /// `flags & 2` string form of `act_type`, which no current scheme uses.
    Str(String),
    /// `act_var`: the variable's declaration field node. This is variable
    /// identity.
    Var(CTId),
    /// The `(void *)-1` placeholder used for the fresh vars of partial type
    /// applications.
    Dummy,
}

/// `struct tl_combinator_tree`. The C `name` member is never read and is
/// omitted.
#[derive(Debug, Clone)]
pub struct CTNode {
    pub act: Act,
    pub left: Option<CTId>,
    pub right: Option<CTId>,
    pub data: CTData,
    pub flags: i64,
    pub ty: CType,
    pub type_len: i32,
    pub type_flags: i64,
}

/// `struct tl_type`.
#[derive(Debug, Clone)]
pub struct TlType {
    pub id: String,
    pub print_id: String,
    pub real_id: Option<String>,
    pub name: u32,
    pub flags: i32,
    pub params_num: i32,
    pub params_types: i64,
    pub constructors: Vec<CtorId>,
}

/// `struct tl_constructor`; functions reuse the same shape.
#[derive(Debug, Clone)]
pub struct TlConstructor {
    pub id: String,
    pub print_id: String,
    pub real_id: Option<String>,
    pub name: u32,
    pub ty: Option<TypeId>,
    pub left: Option<CTId>,
    pub right: Option<CTId>,
}

/// `struct tl_var`.
#[derive(Debug, Clone)]
pub struct TlVar {
    pub id: String,
    pub ptr: CTId,
    pub var_type: i32,
    pub flags: i32,
}

/// The parsed scheme: `struct tl_program` plus the symbol tables the C code
/// keeps in globals. Constructors and functions share one arena; the id maps
/// iterate byte-sorted, like the C treap in-order walks.
#[derive(Debug)]
pub struct TlProgram {
    pub types: Vec<TlType>,
    pub ctors: Vec<TlConstructor>,
    pub type_ids: BTreeMap<String, TypeId>,
    pub ctor_ids: BTreeMap<String, CtorId>,
    pub func_ids: BTreeMap<String, CtorId>,
    pub ct: Vec<CTNode>,
    pub total_types: usize,
    pub total_ctors: usize,
    pub total_funcs: usize,
}

/// Failure of the lexer, the parse-tree stage, or `tl_parse`, carrying the
/// furthest lexer error (as `tl_print_parse_error` would print it) and the
/// collected `TL_ERROR` messages.
#[derive(Debug)]
pub struct TlParseError {
    pub message: String,
    pub errors: Vec<String>,
}

impl fmt::Display for TlParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)?;
        for e in &self.errors {
            write!(f, "\n{e}")?;
        }
        Ok(())
    }
}

impl std::error::Error for TlParseError {}

/// The `#` type's hardcoded id (`tl_type_check`).
const TYPE_NUM_NAME: u32 = 0x7065_9eff;
/// The `Type` type's hardcoded id.
const TYPE_TYPE_NAME: u32 = 0x2cec_f817;

// Flags on `tl_type.flags` (named ones from tl-parser.h where defined).
const TYPE_FINAL: i32 = 1;
const TYPE_PARAMS_UNSET: i32 = 4;
const TYPE_BARE_USED: i32 = 8;
const TYPE_OVERLAPPING: i32 = 16;
const TYPE_EMPTY: i32 = 1 << 10;
const TYPE_DEFAULT_CONSTRUCTOR: i32 = 1 << 25;

/// `FLAG_EXCL` on combinator-tree field nodes.
const FLAG_EXCL: i64 = 1 << 18;

/// The value maps (`struct tree_var_value` treaps): field-node id →
/// (value node, numeric value).
type VarValues = BTreeMap<CTId, (CTId, i64)>;

/// Result shape of `change_first_var`/`change_value_var`, standing in for
/// the C `0` / `(void *)-1` / `(void *)-2` / node-pointer returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChangeRes {
    Err,
    Null,
    Splice,
    Node(CTId),
}

impl ChangeRes {
    fn node(self) -> Option<CTId> {
        match self {
            ChangeRes::Node(id) => Some(id),
            _ => None,
        }
    }
}

impl TlProgram {
    /// Full pipeline: `tl_init_parse_file` → `tl_parse_lex` → `tl_parse`.
    pub fn parse(text: Vec<u8>) -> Result<TlProgram, TlParseError> {
        let mut lexer = Lexer::new(text);
        let root = tl_parse_lex(&mut lexer).ok_or_else(|| TlParseError {
            message: format!("Error in parse:\n{}", lexer.print_parse_error()),
            errors: Vec::new(),
        })?;
        let src = lexer.into_text();
        let mut p = ProgramParser::new(&src);
        if !p.tl_parse(&root) {
            let errors = std::mem::take(&mut p.errors);
            return Err(TlParseError {
                message: "tl_parse failed".to_owned(),
                errors,
            });
        }
        Ok(p.finish())
    }
}

struct ProgramParser<'s> {
    src: &'s [u8],
    types: Vec<TlType>,
    ctors: Vec<TlConstructor>,
    type_ids: BTreeMap<String, TypeId>,
    ctor_ids: BTreeMap<String, CtorId>,
    func_ids: BTreeMap<String, CtorId>,
    ct: Vec<CTNode>,
    vars: Vec<TlVar>,
    var_maps: Vec<BTreeMap<String, VarId>>,
    field_maps: Vec<BTreeSet<String>>,
    last_num_var: [Option<VarId>; 10],
    namespace_level: usize,
    /// Deterministic stand-in for the C `lrand48`-based names of anonymous
    /// `#`/`Type` variables (digit strings never collide with identifiers).
    unnamed_counter: i64,
    buf: Vec<u8>,
    total_types: usize,
    total_ctors: usize,
    total_funcs: usize,
    errors: Vec<String>,
}

impl<'s> ProgramParser<'s> {
    fn new(src: &'s [u8]) -> ProgramParser<'s> {
        ProgramParser {
            src,
            types: Vec::new(),
            ctors: Vec::new(),
            type_ids: BTreeMap::new(),
            ctor_ids: BTreeMap::new(),
            func_ids: BTreeMap::new(),
            ct: Vec::new(),
            vars: Vec::new(),
            var_maps: (0..10).map(|_| BTreeMap::new()).collect(),
            field_maps: (0..10).map(|_| BTreeSet::new()).collect(),
            last_num_var: [None; 10],
            namespace_level: 0,
            unnamed_counter: 0,
            buf: Vec::new(),
            total_types: 0,
            total_ctors: 0,
            total_funcs: 0,
            errors: Vec::new(),
        }
    }

    fn finish(self) -> TlProgram {
        TlProgram {
            types: self.types,
            ctors: self.ctors,
            type_ids: self.type_ids,
            ctor_ids: self.ctor_ids,
            func_ids: self.func_ids,
            ct: self.ct,
            total_types: self.total_types,
            total_ctors: self.total_ctors,
            total_funcs: self.total_funcs,
        }
    }

    fn err(&mut self, msg: impl Into<String>) {
        self.errors.push(msg.into());
    }

    fn text(&self, t: &TreeNode) -> &'s [u8] {
        t.text_of(self.src)
    }

    fn owned(&self, bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    // ----- symbol tables ------------------------------------------------

    /// `tl_is_type_name`: uppercase after the last dot (or at the start);
    /// `#` alone is the nat type.
    fn is_type_name(id: &[u8]) -> bool {
        if id.len() == 1 && id[0] == b'#' {
            return true;
        }
        let mut ok = id[0].is_ascii_uppercase();
        for i in 0..id.len().saturating_sub(1) {
            if id[i] == b'.' {
                ok = id[i + 1].is_ascii_uppercase();
            }
        }
        ok
    }

    fn get_type(&self, id: &[u8]) -> Option<TypeId> {
        self.type_ids.get(&self.owned(id)).copied()
    }

    /// `tl_add_type`; `params_num < 0` marks the arity as not yet known.
    fn add_type(&mut self, id: &[u8], params_num: i32, params_types: i64) -> Option<TypeId> {
        let id_str = self.owned(id);
        if let Some(&existing) = self.type_ids.get(&id_str) {
            if params_num >= 0
                && (self.types[existing].params_num != params_num
                    || self.types[existing].params_types != params_types)
            {
                self.err(format!(
                    "Wrong params_num or types for type {}",
                    self.types[existing].id
                ));
                return None;
            }
            return Some(existing);
        }
        let print_id = dollarize(&id_str);
        let mut t = TlType {
            id: id_str,
            print_id,
            real_id: None,
            name: 0,
            flags: 0,
            params_num: if params_num >= 0 { params_num } else { -1 },
            // Unread while TYPE_PARAMS_UNSET is set; 0 keeps it deterministic.
            params_types: if params_num >= 0 { params_types } else { 0 },
            constructors: Vec::new(),
        };
        if params_num < 0 {
            t.flags |= TYPE_PARAMS_UNSET;
        }
        self.types.push(t);
        let tid = (self.types.len() - 1) as TypeId;
        self.type_ids.insert(self.types[tid].id.clone(), tid);
        self.total_types += 1;
        Some(tid)
    }

    /// `tl_type_set_params`.
    fn type_set_params(&mut self, t: TypeId, x: i32, y: i64) -> bool {
        if self.types[t].flags & TYPE_PARAMS_UNSET != 0 {
            self.types[t].params_num = x;
            self.types[t].params_types = y;
            self.types[t].flags &= !TYPE_PARAMS_UNSET;
        } else if self.types[t].params_num != x || self.types[t].params_types != y {
            self.err(format!("Wrong num of params (type {})", self.types[t].id));
            return false;
        }
        true
    }

    /// `tl_type_finalize`.
    fn type_finalize(&mut self, t: TypeId) {
        self.types[t].flags &= !TYPE_PARAMS_UNSET;
    }

    fn get_constructor(&self, id: &[u8]) -> Option<CtorId> {
        self.ctor_ids.get(&self.owned(id)).copied()
    }

    fn get_function(&self, id: &[u8]) -> Option<CtorId> {
        self.func_ids.get(&self.owned(id)).copied()
    }

    /// Splits an optional `#magic` suffix off a full combinator id and
    /// parses the magic (5-8 lowercase hex digits), like the prologues of
    /// `tl_add_constructor`/`tl_add_function`. `force_magic` (the C
    /// parameter of the same name) means "the id carries no magic".
    fn split_magic(full_id: &[u8], force_magic: bool) -> (usize, u32) {
        let x = if force_magic {
            full_id.len()
        } else {
            full_id
                .iter()
                .position(|&c| c == b'#')
                .unwrap_or(full_id.len())
        };
        let mut magic = 0u32;
        if x < full_id.len() {
            let digits = &full_id[x + 1..];
            assert!(
                (5..=8).contains(&digits.len()),
                "magic must be 5..8 hex digits"
            );
            for &d in digits {
                magic = (magic << 4)
                    + if d.is_ascii_digit() {
                        (d - b'0') as u32
                    } else {
                        (d - b'a' + 10) as u32
                    };
            }
            assert!(magic != 0 && magic != u32::MAX, "magic must be nonzero");
        }
        (x, magic)
    }

    /// `tl_add_constructor`.
    fn add_constructor(&mut self, ty: TypeId, full_id: &[u8], force_magic: bool) -> Option<CtorId> {
        if self.types[ty].flags & TYPE_FINAL != 0 {
            self.err(format!(
                "New constructor for type `{}` after final statement",
                self.types[ty].id
            ));
            return None;
        }
        let (x, magic) = Self::split_magic(full_id, force_magic);
        let id = self.owned(&full_id[..x]);
        if !id.starts_with('_') {
            if self.ctor_ids.contains_key(&id) {
                self.err(format!("Duplicate constructor id `{id}`"));
                return None;
            }
        } else {
            assert!(id.len() == 1, "anonymous constructor must be `_`");
        }
        let print_id = dollarize(&id);
        let c = TlConstructor {
            id: id.clone(),
            print_id,
            real_id: None,
            name: magic,
            ty: Some(ty),
            left: None,
            right: None,
        };
        self.ctors.push(c);
        let cid = (self.ctors.len() - 1) as CtorId;
        self.types[ty].constructors.push(cid);
        if !id.starts_with('_') {
            self.ctor_ids.insert(id, cid);
        } else {
            self.types[ty].flags |= TYPE_DEFAULT_CONSTRUCTOR;
        }
        self.total_ctors += 1;
        Some(cid)
    }

    /// `tl_add_function`.
    fn add_function(
        &mut self,
        ty: Option<TypeId>,
        full_id: &[u8],
        force_magic: bool,
    ) -> Option<CtorId> {
        let (x, magic) = Self::split_magic(full_id, force_magic);
        let id = self.owned(&full_id[..x]);
        if self.func_ids.contains_key(&id) {
            self.err(format!("Duplicate function id `{id}`"));
            return None;
        }
        let print_id = dollarize(&id);
        let c = TlConstructor {
            id: id.clone(),
            print_id,
            real_id: None,
            name: magic,
            ty,
            left: None,
            right: None,
        };
        self.ctors.push(c);
        let cid = (self.ctors.len() - 1) as CtorId;
        self.func_ids.insert(id, cid);
        self.total_funcs += 1;
        Some(cid)
    }

    /// `tl_add_field`.
    fn add_field(&mut self, id: &str) -> bool {
        let lvl = self.namespace_level;
        if self.field_maps[lvl].contains(id) {
            return false;
        }
        self.field_maps[lvl].insert(id.to_owned());
        true
    }

    /// `tl_clear_fields`.
    fn clear_fields(&mut self) {
        let lvl = self.namespace_level;
        self.field_maps[lvl].clear();
    }

    /// `tl_add_var` (the arena entry outlives the scope like the tree nodes
    /// that reference it by id).
    fn add_var(&mut self, id: &str, ptr: CTId, var_type: i32) -> Option<VarId> {
        let lvl = self.namespace_level;
        if self.var_maps[lvl].contains_key(id) {
            return None;
        }
        self.vars.push(TlVar {
            id: id.to_owned(),
            ptr,
            var_type,
            flags: 0,
        });
        let vid = (self.vars.len() - 1) as VarId;
        self.var_maps[lvl].insert(id.to_owned(), vid);
        if var_type != 0 {
            self.last_num_var[lvl] = Some(vid);
        }
        Some(vid)
    }

    /// `tl_clear_vars`.
    fn clear_vars(&mut self) {
        let lvl = self.namespace_level;
        self.var_maps[lvl].clear();
        self.last_num_var[lvl] = None;
    }

    /// `tl_get_var`: innermost scope first.
    fn get_var(&self, id: &[u8]) -> Option<VarId> {
        let id = self.owned(id);
        for lvl in (0..=self.namespace_level).rev() {
            if let Some(&v) = self.var_maps[lvl].get(&id) {
                return Some(v);
            }
        }
        None
    }

    /// `namespace_push`.
    fn namespace_push(&mut self) {
        self.namespace_level += 1;
        assert!(self.namespace_level < 10);
        self.clear_vars();
        self.clear_fields();
    }

    /// `namespace_pop`.
    fn namespace_pop(&mut self) {
        self.namespace_level -= 1;
    }

    // ----- combinator tree ----------------------------------------------

    /// `alloc_ctree_node`: a zeroed node.
    fn ct_new(&mut self) -> CTId {
        self.ct.push(CTNode {
            act: Act::Var,
            left: None,
            right: None,
            data: CTData::None,
            flags: 0,
            ty: CType::Type,
            type_len: 0,
            type_flags: 0,
        });
        (self.ct.len() - 1) as CTId
    }

    /// `tl_tree_dup`.
    fn tree_dup(&mut self, t: Option<CTId>) -> Option<CTId> {
        let t = t?;
        let src = self.ct[t].clone();
        let new = self.ct_new();
        self.ct[new] = src;
        let l = self.tree_dup(self.ct[new].left);
        let r = self.tree_dup(self.ct[new].right);
        self.ct[new].left = l;
        self.ct[new].right = r;
        Some(new)
    }

    /// `tl_tree_get_type`: the head type of a type tree, `None` for arrays
    /// and var-headed applications.
    fn tree_get_type(&self, t: CTId) -> Option<TypeId> {
        assert!(self.ct[t].ty == CType::Type);
        let mut t = t;
        loop {
            let n = &self.ct[t];
            if n.act == Act::Array {
                return None;
            }
            match n.left {
                Some(l) => {
                    t = l;
                    assert!(self.ct[t].ty == CType::Type);
                }
                None => break,
            }
        }
        let n = &self.ct[t];
        assert!(
            n.act == Act::Type || n.act == Act::Var || n.act == Act::Array,
            "unexpected head act"
        );
        match (n.act, &n.data) {
            (Act::Type, CTData::Type(id)) => Some(*id),
            _ => None,
        }
    }

    /// `tl_finish_subtree`.
    fn finish_subtree(&mut self, r: CTId) -> bool {
        if self.ct[r].ty != CType::Type {
            return true;
        }
        if self.ct[r].type_len >= 0 {
            if self.ct[r].type_len > 0 {
                self.err("Not enough params");
                return false;
            }
            return true;
        }
        self.finish_subtree_inner(r, 0, 0)
    }

    /// `_tl_finish_subtree`.
    fn finish_subtree_inner(&mut self, r: CTId, x: i32, y: i64) -> bool {
        assert!(self.ct[r].ty == CType::Type);
        assert!(self.ct[r].type_len < 0);
        assert!(self.ct[r].act == Act::Arg || self.ct[r].act == Act::Type);
        self.ct[r].type_len = x;
        self.ct[r].type_flags = y;
        if self.ct[r].act == Act::Type {
            let t = match self.ct[r].data {
                CTData::Type(t) => t,
                _ => panic!("act_type without type data"),
            };
            return self.type_set_params(t, x, y);
        }
        let right = self.ct[r].right.expect("act_arg without right");
        let rn = &self.ct[right];
        assert!(
            (rn.ty == CType::Type && rn.type_len == 0)
                || rn.ty == CType::Num
                || rn.ty == CType::NumValue
        );
        let is_num = rn.ty == CType::Num || rn.ty == CType::NumValue;
        let left = self.ct[r].left.expect("act_arg without left");
        self.finish_subtree_inner(left, x + 1, y * 2 + is_num as i64)
    }

    /// `tl_union`.
    fn union_(&mut self, l: Option<CTId>, r: Option<CTId>) -> Option<CTId> {
        let (Some(l), Some(r)) = (l, r) else {
            return l.or(r);
        };
        match self.ct[l].ty {
            CType::Num => {
                if self.ct[r].ty != CType::NumValue {
                    self.err("Union: type mistmatch");
                    return None;
                }
                self.ct[l].type_flags += self.ct[r].type_flags;
                Some(l)
            }
            CType::NumValue => {
                if self.ct[r].ty != CType::NumValue && self.ct[r].ty != CType::Num {
                    self.err("Union: type mistmatch");
                    return None;
                }
                self.ct[r].type_flags += self.ct[l].type_flags;
                Some(r)
            }
            CType::ListItem | CType::List => {
                if self.ct[r].ty != CType::ListItem {
                    self.err("Union: type mistmatch");
                    return None;
                }
                let v = self.ct_new();
                self.ct[v].left = Some(l);
                self.ct[v].right = Some(r);
                self.ct[v].ty = CType::List;
                self.ct[v].act = Act::Union;
                Some(v)
            }
            CType::Type => {
                if self.ct[l].type_len == 0 {
                    self.err("Arguments number exceeds type arity");
                    return None;
                }
                if !matches!(self.ct[r].ty, CType::Num | CType::Type | CType::NumValue) {
                    self.err("Union: type mistmatch");
                    return None;
                }
                if self.ct[r].type_len < 0 && !self.finish_subtree(r) {
                    return None;
                }
                if self.ct[r].type_len > 0 {
                    self.err("Argument type must have full number of arguments");
                    return None;
                }
                let r_is_num = self.ct[r].ty == CType::Num || self.ct[r].ty == CType::NumValue;
                if self.ct[l].type_len > 0 && (self.ct[l].type_flags & 1) != r_is_num as i64 {
                    self.err("Argument types mistmatch");
                    return None;
                }
                let v = self.ct_new();
                self.ct[v].left = Some(l);
                self.ct[v].right = Some(r);
                self.ct[v].ty = CType::Type;
                self.ct[v].act = Act::Arg;
                self.ct[v].type_len = if self.ct[l].type_len > 0 {
                    self.ct[l].type_len - 1
                } else {
                    -1
                };
                self.ct[v].type_flags = self.ct[l].type_flags >> 1;
                Some(v)
            }
        }
    }

    /// `tl_mark_vars`.
    fn mark_vars(&mut self, t: Option<CTId>) {
        let Some(t) = t else { return };
        if self.ct[t].act == Act::Var {
            let field = match self.ct[t].data {
                CTData::Var(f) => f,
                _ => panic!("act_var without field data"),
            };
            let name = match &self.ct[field].data {
                CTData::Str(s) => s.clone(),
                _ => panic!("var field without name"),
            };
            let v = self
                .get_var(name.as_bytes())
                .expect("tl_mark_vars var lookup");
            self.vars[v].flags |= 1;
        }
        let l = self.ct[t].left;
        let r = self.ct[t].right;
        self.mark_vars(l);
        self.mark_vars(r);
    }

    // ----- canonical-string buffer --------------------------------------

    /// `tl_buf_add_string` (leading space).
    fn buf_add_string(&mut self, s: &[u8]) {
        self.buf.push(b' ');
        self.buf.extend_from_slice(s);
    }

    /// `tl_buf_add_string_nospace`.
    fn buf_add_string_nospace(&mut self, s: &[u8]) {
        self.buf.extend_from_slice(s);
    }

    /// `tl_buf_add_string_q`.
    fn buf_add_string_q(&mut self, s: &[u8], space: bool) {
        if space {
            self.buf_add_string(s);
        } else {
            self.buf_add_string_nospace(s);
        }
    }

    /// `tl_buf_add_tree`.
    fn buf_add_tree(&mut self, t: Option<CTId>, space: bool) {
        let Some(t) = t else { return };
        let node = self.ct[t].clone();
        match node.act {
            Act::QuestionMark => {
                self.buf_add_string_q(b"?", space);
            }
            Act::Type => {
                let mut space = space;
                if node.flags & 1 != 0 && node.flags & 4 == 0 {
                    self.buf_add_string_q(b"%", space);
                    space = false;
                }
                if node.flags & 2 != 0 {
                    let s = match &node.data {
                        CTData::Str(s) => s.clone().into_bytes(),
                        _ => panic!("act_type flags&2 without string data"),
                    };
                    self.buf_add_string_q(&s, space);
                } else {
                    // Node `flags & 4` marks a single-constructor type
                    // referenced through its constructor (`tl_parse_ident`'s
                    // `flags |= 5`): print that constructor's id.
                    let ty = match &node.data {
                        CTData::Type(id) => *id,
                        _ => panic!("act_type without type data"),
                    };
                    let s = if node.flags & 4 != 0 {
                        assert!(self.types[ty].constructors.len() == 1);
                        let c = self.types[ty].constructors[0];
                        self.ctors[c]
                            .real_id
                            .clone()
                            .unwrap_or_else(|| self.ctors[c].id.clone())
                    } else {
                        self.types[ty]
                            .real_id
                            .clone()
                            .unwrap_or_else(|| self.types[ty].id.clone())
                    };
                    self.buf_add_string_q(s.as_bytes(), space);
                }
            }
            Act::Field => {
                let mut space = space;
                if let CTData::Str(name) = &node.data {
                    self.buf_add_string_q(name.as_bytes(), space);
                    space = false;
                    self.buf_add_string_q(b":", false);
                }
                self.buf_add_tree(node.left, space);
                self.buf_add_tree(node.right, true);
            }
            Act::Union | Act::Arg => {
                self.buf_add_tree(node.left, space);
                self.buf_add_tree(node.right, true);
            }
            Act::Var => {
                match &node.data {
                    CTData::Dummy => return,
                    CTData::Var(field) => {
                        let name = match &self.ct[*field].data {
                            CTData::Str(s) => s.clone().into_bytes(),
                            _ => panic!("var field without name"),
                        };
                        self.buf_add_string_q(&name, space);
                    }
                    _ => panic!("act_var without field data"),
                }
                if node.ty == CType::Num && node.type_flags != 0 {
                    self.buf_add_string_q(format!("+{}", node.type_flags).as_bytes(), false);
                }
            }
            Act::Array => {
                let mut space = space;
                if node.left.is_some_and(|l| self.ct[l].flags & 128 == 0) {
                    self.buf_add_tree(node.left, space);
                    space = false;
                    self.buf_add_string_q(b"*", space);
                }
                self.buf_add_string_q(b"[", space);
                self.buf_add_tree(node.right, true);
                self.buf_add_string_q(b"]", true);
            }
            Act::Plus => {
                self.buf_add_tree(node.left, space);
                self.buf_add_string_q(b"+", false);
                self.buf_add_tree(node.right, false);
            }
            Act::NatConst => {
                let s = format!("{}", node.type_flags).into_bytes();
                self.buf_add_string_q(&s, space);
            }
            Act::OptField => {
                let left = node.left.expect("act_opt_field without left");
                let field = match self.ct[left].data {
                    CTData::Var(f) => f,
                    _ => panic!("act_opt_field left without var data"),
                };
                let name = match &self.ct[field].data {
                    CTData::Str(s) => s.clone().into_bytes(),
                    _ => panic!("var field without name"),
                };
                self.buf_add_string_q(&name, space);
                self.buf_add_string_q(b".", false);
                let digits = format!("{}", self.ct[left].type_flags).into_bytes();
                self.buf_add_string_q(&digits, false);
                self.buf_add_string_q(b"?", false);
                self.buf_add_tree(node.right, false);
            }
        }
    }

    /// `tl_count_combinator_name`.
    fn count_combinator_name(&mut self, c: CtorId) -> u32 {
        self.buf.clear();
        let head = self.ctors[c]
            .real_id
            .clone()
            .unwrap_or_else(|| self.ctors[c].id.clone())
            .into_bytes();
        self.buf_add_string_nospace(&head);
        self.buf_add_tree(self.ctors[c].left, true);
        self.buf_add_string(b"=");
        self.buf_add_tree(self.ctors[c].right, true);
        if self.ctors[c].name == 0 {
            self.ctors[c].name = compute_crc32(&self.buf);
        }
        self.ctors[c].name
    }

    // ----- tree → combinator tree walkers -------------------------------

    /// `tl_parse_any_term`.
    fn parse_any_term(&mut self, t: &TreeNode, s: i32) -> Option<CTId> {
        match t.ty {
            TreeType::TypeTerm => self.parse_type_term(t, s),
            TreeType::NatTerm => self.parse_nat_term(t, s),
            TreeType::Term => self.parse_term(t, s),
            TreeType::Expr => self.parse_expr(t, s),
            TreeType::Subexpr => self.parse_subexpr(t, s),
            TreeType::NatConst => self.parse_nat_const(t, s),
            TreeType::TypeIdent | TreeType::VarIdent => self.parse_ident(t, s),
            _ => panic!("unexpected term node {:?}", t.ty),
        }
    }

    /// `tl_parse_term`.
    fn parse_term(&mut self, t: &TreeNode, s: i32) -> Option<CTId> {
        assert!(t.ty == TreeType::Term);
        let mut i = 0;
        let mut s = s;
        while i < t.children.len() && t.children[i].ty == TreeType::Percent {
            i += 1;
            s += 1;
        }
        assert!(i < t.children.len());
        let mut l: Option<CTId> = None;
        while i < t.children.len() {
            let z = self.parse_any_term(&t.children[i], s)?;
            l = Some(self.union_(l, Some(z))?);
            s = 0;
            i += 1;
        }
        l
    }

    /// `tl_parse_type_term`.
    fn parse_type_term(&mut self, t: &TreeNode, s: i32) -> Option<CTId> {
        assert!(t.ty == TreeType::TypeTerm && t.children.len() == 1);
        let z = self.parse_term(&t.children[0], s)?;
        if self.ct[z].ty != CType::Type {
            self.err(format!("type_term: found type {:?}", self.ct[z].ty));
            return None;
        }
        Some(z)
    }

    /// `tl_parse_nat_term`.
    fn parse_nat_term(&mut self, t: &TreeNode, s: i32) -> Option<CTId> {
        assert!(t.ty == TreeType::NatTerm && t.children.len() == 1);
        let z = self.parse_term(&t.children[0], s)?;
        if self.ct[z].ty != CType::Num && self.ct[z].ty != CType::NumValue {
            self.err(format!("nat_term: found type {:?}", self.ct[z].ty));
            return None;
        }
        Some(z)
    }

    /// `tl_parse_subexpr`.
    fn parse_subexpr(&mut self, t: &TreeNode, s: i32) -> Option<CTId> {
        assert!(t.ty == TreeType::Subexpr && !t.children.is_empty());
        let mut s = s;
        let mut l: Option<CTId> = None;
        for child in &t.children {
            let z = self.parse_any_term(child, s)?;
            l = Some(self.union_(l, Some(z))?);
            s = 0;
        }
        l
    }

    /// `tl_parse_expr`.
    fn parse_expr(&mut self, t: &TreeNode, s: i32) -> Option<CTId> {
        assert!(t.ty == TreeType::Expr && !t.children.is_empty());
        let mut s = s;
        let mut l: Option<CTId> = None;
        for child in &t.children {
            let z = self.parse_subexpr(child, s)?;
            l = Some(self.union_(l, Some(z))?);
            s = 0;
        }
        l
    }

    /// `tl_parse_nat_const`.
    fn parse_nat_const(&mut self, t: &TreeNode, s: i32) -> Option<CTId> {
        assert!(t.ty == TreeType::NatConst && t.children.is_empty());
        if s > 0 {
            self.err("Nat const can not preceed with %");
            return None;
        }
        let l = self.ct_new();
        self.ct[l].act = Act::NatConst;
        self.ct[l].ty = CType::NumValue;
        let mut x: i64 = 0;
        for &d in self.text(t) {
            x = x * 10 + (d - b'0') as i64;
        }
        self.ct[l].type_flags = x;
        Some(l)
    }

    /// `tl_parse_ident`.
    fn parse_ident(&mut self, t: &TreeNode, s: i32) -> Option<CTId> {
        assert!(
            t.ty == TreeType::TypeIdent
                || t.ty == TreeType::VarIdent
                || t.ty == TreeType::BoxedTypeIdent
        );
        assert!(t.children.is_empty());
        let text = self.text(t);
        if let Some(v) = self.get_var(text) {
            let l = self.ct_new();
            self.ct[l].act = Act::Var;
            self.ct[l].ty = if self.vars[v].var_type != 0 {
                CType::Num
            } else {
                CType::Type
            };
            if self.ct[l].ty == CType::Num && s > 0 {
                self.err("Nat var can not preceed with %");
                return None;
            } else if s > 0 {
                self.ct[l].flags |= 1;
            }
            self.ct[l].type_len = 0;
            self.ct[l].type_flags = 0;
            self.ct[l].data = CTData::Var(self.vars[v].ptr);
            return Some(l);
        }
        if let Some(c) = self.get_constructor(text) {
            let ty = self.ctors[c].ty.expect("constructor without type");
            if self.types[ty].constructors.len() != 1 {
                self.err("Constructor can be used only if it is the only constructor of the type");
                return None;
            }
            self.types[ty].flags |= TYPE_FINAL;
            let l = self.ct_new();
            self.ct[l].act = Act::Type;
            self.ct[l].flags |= 5;
            self.ct[l].data = CTData::Type(ty);
            self.ct[l].ty = CType::Type;
            self.ct[l].type_len = self.types[ty].params_num;
            self.ct[l].type_flags = self.types[ty].params_types;
            return Some(l);
        }
        if Self::is_type_name(text) {
            let ty = self.add_type(text, -1, 0)?;
            let l = self.ct_new();
            if s > 0 {
                self.ct[l].flags |= 1;
                self.types[ty].flags |= TYPE_BARE_USED;
            }
            self.ct[l].act = Act::Type;
            self.ct[l].data = CTData::Type(ty);
            self.ct[l].ty = CType::Type;
            self.ct[l].type_len = self.types[ty].params_num;
            self.ct[l].type_flags = self.types[ty].params_types;
            return Some(l);
        }
        self.err(format!(
            "Not a type/var ident `{}`",
            String::from_utf8_lossy(text)
        ));
        None
    }

    /// `tl_parse_multiplicity`.
    fn parse_multiplicity(&mut self, t: &TreeNode) -> Option<CTId> {
        assert!(t.ty == TreeType::Multiplicity && t.children.len() == 1);
        self.parse_nat_term(&t.children[0], 0)
    }

    /// `tl_parse_opt_args`: the `{name:Type …}` block.
    fn parse_opt_args(&mut self, t: &TreeNode) -> Option<CTId> {
        assert!(t.ty == TreeType::OptArgs && t.children.len() >= 2);
        let nc = t.children.len();
        let r = self.parse_type_term(&t.children[nc - 1], 0)?;
        assert!(self.finish_subtree(r));
        assert!(self.ct[r].ty == CType::Type && self.ct[r].type_len == 0);
        let tt = match self.tree_get_type(r) {
            Some(ty) if self.types[ty].id == "#" => 1,
            Some(ty) if self.types[ty].id == "Type" => 0,
            _ => -1,
        };
        if tt < 0 {
            self.err("Optargs can be only of type # or Type");
            return None;
        }
        for child in &t.children[..nc - 1] {
            if child.ty != TreeType::VarIdent {
                self.err("Variable name expected");
                return None;
            }
            let text = self.text(child);
            if text.len() == 1 && text[0] == b'_' {
                self.err("Variables can not be unnamed");
                return None;
            }
        }
        let mut h: Option<CTId> = None;
        for i in 0..=nc - 2 {
            let s = self.ct_new();
            self.ct[s].left = if i == nc - 2 {
                Some(r)
            } else {
                self.tree_dup(Some(r))
            };
            self.ct[s].right = None;
            self.ct[s].ty = CType::ListItem;
            self.ct[s].type_len = 0;
            self.ct[s].act = Act::Field;
            let name = self.owned(self.text(&t.children[i]));
            self.ct[s].data = CTData::Str(name.clone());
            let _ = self.add_var(&name, s, tt);
            self.ct[s].flags = 33;
            h = Some(self.union_(h, Some(s))?);
        }
        h
    }

    /// `tl_parse_args2`: `[args]` arrays, with optional `name:` and
    /// multiplicity.
    fn parse_args2(&mut self, t: &TreeNode) -> Option<CTId> {
        assert!(t.ty == TreeType::Args2 && !t.children.is_empty());
        let nc = t.children.len();
        let mut r: Option<CTId> = None;
        let mut x = 0usize;
        let mut field_name: Option<String> = None;
        if t.children[x].ty == TreeType::VarIdentOpt || t.children[x].ty == TreeType::VarIdent {
            let name = self.owned(self.text(&t.children[x]));
            if !self.add_field(&name) {
                self.err(format!("Duplicate field name {name}"));
                return None;
            }
            field_name = Some(name);
            x += 1;
        }
        let l;
        // The C code reads `T->c[x]` unconditionally here; the bounds check
        // only diverges where C would read out of bounds (empty `[ ]`).
        if x < nc && t.children[x].ty == TreeType::Multiplicity {
            l = self.parse_multiplicity(&t.children[x])?;
            x += 1;
        } else {
            let Some(v) = self.last_num_var[self.namespace_level] else {
                self.err("Expected multiplicity or nat var");
                return None;
            };
            l = self.ct_new();
            self.ct[l].act = Act::Var;
            self.ct[l].ty = CType::Num;
            self.ct[l].flags |= 128;
            self.ct[l].type_len = 0;
            self.ct[l].type_flags = 0;
            let ptr = self.vars[v].ptr;
            self.ct[l].data = CTData::Var(ptr);
            self.ct[ptr].flags |= 256;
        }
        self.namespace_push();
        while x < nc {
            let z = self.parse_args(&t.children[x])?;
            r = Some(self.union_(r, Some(z))?);
            x += 1;
        }
        self.namespace_pop();
        let s = self.ct_new();
        self.ct[s].ty = CType::Type;
        self.ct[s].type_len = 0;
        self.ct[s].act = Act::Array;
        self.ct[s].left = Some(l);
        self.ct[s].right = r;
        let h = self.ct_new();
        self.ct[h].ty = CType::ListItem;
        self.ct[h].act = Act::Field;
        self.ct[h].left = Some(s);
        self.ct[h].right = None;
        self.ct[h].data = field_name.map(CTData::Str).unwrap_or(CTData::None);
        self.ct[h].type_len = 0;
        Some(h)
    }

    /// `tl_parse_args134`: args1 (parens), args3 (`name:type`), and args4
    /// (bare type) all share this shape.
    fn parse_args134(&mut self, t: &TreeNode) -> Option<CTId> {
        assert!(t.ty == TreeType::Args1 || t.ty == TreeType::Args3 || t.ty == TreeType::Args4);
        let nc = t.children.len();
        assert!(nc >= 1);
        let mut r = self.parse_type_term(&t.children[nc - 1], 0)?;
        assert!(self.finish_subtree(r));
        assert!(self.ct[r].ty == CType::Type && self.ct[r].type_len == 0);
        let tt = match self.tree_get_type(r) {
            Some(ty) if self.types[ty].id == "#" => 1,
            Some(ty) if self.types[ty].id == "Type" => 0,
            _ => -1,
        };
        let mut last: isize = nc as isize - 2;
        let mut excl = false;
        if last >= 0 && t.children[last as usize].ty == TreeType::Exclam {
            excl = true;
            self.mark_vars(Some(r));
            last -= 1;
        }
        if last >= 0 && t.children[last as usize].ty == TreeType::OptionalArgDef {
            let od = &t.children[last as usize];
            assert!(od.children.len() == 2);
            let e = self.ct_new();
            self.ct[e].ty = CType::Type;
            self.ct[e].act = Act::OptField;
            let left = self.parse_ident(&od.children[0], 0)?;
            self.ct[e].left = Some(left);
            let mut bits: i64 = 0;
            for &d in self.text(&od.children[1]) {
                bits = bits * 10 + (d - b'0') as i64;
            }
            self.ct[left].type_flags = bits;
            self.ct[e].type_flags = self.ct[r].type_flags;
            self.ct[e].type_len = self.ct[r].type_len;
            self.ct[e].right = Some(r);
            r = e;
            last -= 1;
        }
        for child in &t.children[..last.max(0) as usize] {
            if child.ty != TreeType::VarIdent && child.ty != TreeType::VarIdentOpt {
                self.err("Variable name expected");
                return None;
            }
        }
        let mut h: Option<CTId> = None;
        let mut i: isize = if last >= 0 { 0 } else { -1 };
        while i <= last {
            let s = self.ct_new();
            self.ct[s].left = if i == last {
                Some(r)
            } else {
                self.tree_dup(Some(r))
            };
            self.ct[s].right = None;
            self.ct[s].ty = CType::ListItem;
            self.ct[s].type_len = 0;
            self.ct[s].act = Act::Field;
            self.ct[s].data = if i >= 0 {
                CTData::Str(self.owned(self.text(&t.children[i as usize])))
            } else {
                CTData::None
            };
            if excl {
                self.ct[s].flags |= FLAG_EXCL;
            }
            if i >= 0 {
                let text = self.text(&t.children[i as usize]);
                if text.len() >= 2 || text[0] != b'_' {
                    let name = self.owned(text);
                    if !self.add_field(&name) {
                        self.err(format!("Duplicate field name {name}"));
                        return None;
                    }
                }
            }
            if tt >= 0 {
                let name = match &self.ct[s].data {
                    CTData::Str(n) => n.clone(),
                    _ => {
                        // C invents a random decimal name; digits can never
                        // collide with identifiers.
                        self.unnamed_counter += 1;
                        format!("{}", self.unnamed_counter)
                    }
                };
                let v = self.add_var(&name, s, tt)?;
                self.vars[v].flags |= 2;
            }
            h = Some(self.union_(h, Some(s))?);
            i += 1;
        }
        h
    }

    /// `tl_parse_args`.
    fn parse_args(&mut self, t: &TreeNode) -> Option<CTId> {
        assert!(t.ty == TreeType::Args && t.children.len() == 1);
        match t.children[0].ty {
            TreeType::Args1 | TreeType::Args3 | TreeType::Args4 => {
                self.parse_args134(&t.children[0])
            }
            TreeType::Args2 => self.parse_args2(&t.children[0]),
            _ => panic!("unexpected args node"),
        }
    }

    /// `tl_parse_result_type`.
    fn parse_result_type(&mut self, t: &TreeNode) -> Option<CTId> {
        assert!(t.ty == TreeType::ResultType && !t.children.is_empty() && t.children.len() <= 64);
        let nc = t.children.len();
        let mut l;
        if self.get_var(self.text(&t.children[0])).is_some() {
            if nc != 1 {
                self.err("Variable can not take params");
                return None;
            }
            l = self.ct_new();
            self.ct[l].act = Act::Var;
            self.ct[l].ty = CType::Type;
            let v = self.get_var(self.text(&t.children[0])).unwrap();
            if self.vars[v].var_type != 0 {
                self.err("Type mistmatch");
                return None;
            }
            self.ct[l].data = CTData::Var(self.vars[v].ptr);
        } else {
            l = self.ct_new();
            self.ct[l].act = Act::Type;
            self.ct[l].ty = CType::Type;
            let ty = self.add_type(self.text(&t.children[0]), -1, 0)?;
            self.ct[l].type_len = self.types[ty].params_num;
            self.ct[l].type_flags = self.types[ty].params_types;
            self.ct[l].data = CTData::Type(ty);
            let mut cur = l;
            for child in &t.children[1..] {
                let z = self.parse_any_term(child, 0)?;
                cur = self.union_(Some(cur), Some(z))?;
                let right = self.ct[cur].right.expect("arg without right");
                let rn = &self.ct[right];
                assert!(
                    rn.ty == CType::Num
                        || rn.ty == CType::NumValue
                        || (rn.ty == CType::Type && rn.type_len == 0)
                );
            }
            l = cur;
        }
        if !self.finish_subtree(l) {
            return None;
        }
        self.mark_vars(Some(l));
        Some(l)
    }

    // ----- declarations -------------------------------------------------

    /// `tl_parse_combinator_decl`.
    fn parse_combinator_decl(&mut self, t: &TreeNode, fun: bool) -> bool {
        assert!(t.ty == TreeType::CombinatorDecl && t.children.len() >= 3);
        self.namespace_level = 0;
        self.clear_vars();
        self.clear_fields();
        let nc = t.children.len();
        let mut l: Option<CTId> = None;
        let mut i = 1usize;
        while i < nc - 2 && t.children[i].ty == TreeType::OptArgs {
            let Some(z) = self.parse_opt_args(&t.children[i]) else {
                return false;
            };
            l = match self.union_(l, Some(z)) {
                Some(v) => Some(v),
                None => return false,
            };
            i += 1;
        }
        while i < nc - 2 && t.children[i].ty == TreeType::Args {
            let Some(z) = self.parse_args(&t.children[i]) else {
                return false;
            };
            l = match self.union_(l, Some(z)) {
                Some(v) => Some(v),
                None => return false,
            };
            i += 1;
        }
        assert!(i == nc - 2 && t.children[i].ty == TreeType::Equals);
        i += 1;
        let Some(r) = self.parse_result_type(&t.children[i]) else {
            return false;
        };
        let ty = self.tree_get_type(r);
        if !fun && ty.is_none() {
            self.err("Only functions can return variables");
        }
        assert!(ty.is_some() || fun);
        assert!(self.namespace_level == 0);
        let mut ok = true;
        for &v in self.var_maps[0].values() {
            ok = ok && self.vars[v].flags & 3 != 0;
        }
        if !ok {
            self.err("Not all variables are used in right side");
            return false;
        }
        let c0 = self.text(&t.children[0]);
        if self.get_constructor(c0).is_some() || self.get_function(c0).is_some() {
            self.err(format!(
                "Duplicate combinator id {}",
                String::from_utf8_lossy(c0)
            ));
            return false;
        }
        let c = if !fun {
            self.add_constructor(ty.expect("constructor type"), c0, false)
        } else {
            self.add_function(ty, c0, false)
        };
        let Some(c) = c else { return false };
        self.ctors[c].left = l;
        self.ctors[c].right = Some(r);
        if self.ctors[c].name == 0 {
            self.count_combinator_name(c);
        }
        true
    }

    // ----- var values / unification -------------------------------------

    /// `tl_set_var_value`.
    fn set_var_value(t: &mut VarValues, var: CTId, value: CTId) {
        t.insert(var, (value, 0));
    }

    /// `tl_set_var_value_num`.
    fn set_var_value_num(t: &mut VarValues, var: CTId, value: CTId, num_value: i64) {
        t.insert(var, (value, num_value));
    }

    /// `tl_get_var_value`; `None` also covers the C `(void *)-1` key, which
    /// never has an entry.
    fn get_var_value(t: &VarValues, var: &CTData) -> Option<CTId> {
        match var {
            CTData::Var(f) => t.get(f).map(|v| v.0),
            _ => None,
        }
    }

    /// `tl_get_var_value_num`.
    fn get_var_value_num(t: &VarValues, var: &CTData) -> i64 {
        match var {
            CTData::Var(f) => t.get(f).map_or(0, |v| v.1),
            _ => 0,
        }
    }

    /// `change_var_ptrs`: rewires a duplicated tree's var references to the
    /// duplicated field nodes, recording original→dup pairs in `v`.
    fn change_var_ptrs(&mut self, o: Option<CTId>, d: Option<CTId>, v: &mut VarValues) {
        let (Some(o), Some(d)) = (o, d) else {
            assert!(o.is_none() && d.is_none());
            return;
        };
        if self.ct[o].act == Act::Field {
            let left = self.ct[o].left.expect("field without left");
            if let Some(t) = self.tree_get_type(left)
                && (self.types[t].id == "#" || self.types[t].id == "Type")
            {
                Self::set_var_value(v, o, d);
            }
        }
        if self.ct[o].act == Act::Var {
            assert_eq!(self.ct[d].data, self.ct[o].data, "dup diverged");
            let val = Self::get_var_value(v, &self.ct[o].data).expect("var value");
            self.ct[d].data = CTData::Var(val);
        }
        let (ol, or_) = (self.ct[o].left, self.ct[o].right);
        let (dl, dr) = (self.ct[d].left, self.ct[d].right);
        self.change_var_ptrs(ol, dl, v);
        self.change_var_ptrs(or_, dr, v);
    }

    /// `change_first_var`.
    fn change_first_var(&mut self, o: Option<CTId>, x: &mut Option<CTId>, y: CTId) -> ChangeRes {
        let Some(o) = o else { return ChangeRes::Null };
        if self.ct[o].act == Act::Field && x.is_none() {
            let left = self.ct[o].left.expect("field without left");
            if let Some(t) = self.tree_get_type(left) {
                if self.types[t].id == "#" {
                    if self.ct[y].ty != CType::Num && self.ct[y].ty != CType::NumValue {
                        self.err("change_var: Type mistmatch");
                        return ChangeRes::Err;
                    }
                    *x = Some(o);
                    return ChangeRes::Splice;
                }
                if self.types[t].id == "Type" {
                    if self.ct[y].ty != CType::Type || self.ct[y].type_len != 0 {
                        self.err("change_var: Type mistmatch");
                        return ChangeRes::Err;
                    }
                    *x = Some(o);
                    return ChangeRes::Splice;
                }
            }
        }
        if self.ct[o].act == Act::Var && x.is_some_and(|k| self.ct[o].data == CTData::Var(k)) {
            let r = self.tree_dup(Some(y)).expect("dup of node");
            if self.ct[o].ty == CType::Num || self.ct[o].ty == CType::NumValue {
                self.ct[r].type_flags += self.ct[o].type_flags;
            }
            return ChangeRes::Node(r);
        }
        let t_left = self.change_first_var(self.ct[o].left, x, y);
        if t_left == ChangeRes::Err {
            return ChangeRes::Err;
        }
        if t_left == ChangeRes::Splice {
            return match self.change_first_var(self.ct[o].right, x, y) {
                ChangeRes::Err => ChangeRes::Err,
                ChangeRes::Splice | ChangeRes::Null => ChangeRes::Splice,
                ChangeRes::Node(n) => ChangeRes::Node(n),
            };
        }
        if let ChangeRes::Node(n) = t_left {
            self.ct[o].left = Some(n);
        }
        let t_right = self.change_first_var(self.ct[o].right, x, y);
        if t_right == ChangeRes::Err {
            return ChangeRes::Err;
        }
        if t_right == ChangeRes::Splice {
            // C returns `O->left` verbatim; a null left means failure.
            return match self.ct[o].left {
                Some(l) => ChangeRes::Node(l),
                None => ChangeRes::Err,
            };
        }
        if let ChangeRes::Node(n) = t_right {
            self.ct[o].right = Some(n);
        }
        ChangeRes::Node(o)
    }

    /// `check_nat_val` folded over the value map.
    fn check_nat_vals(&self, t: &VarValues) -> bool {
        for &(val, num) in t.values() {
            let mut x = num;
            let mut l = val;
            if self.ct[l].ty == CType::Type {
                continue;
            }
            loop {
                if self.ct[l].ty == CType::NumValue {
                    if x + self.ct[l].type_flags < 0 {
                        return false;
                    }
                    break;
                }
                assert!(self.ct[l].ty == CType::Num, "nat walk on non-num");
                x += self.ct[l].type_flags;
                x += Self::get_var_value_num(t, &self.ct[l].data);
                match Self::get_var_value(t, &self.ct[l].data) {
                    Some(nl) => l = nl,
                    None => break,
                }
            }
        }
        true
    }

    /// `check_constructors_equal`.
    fn check_constructors_equal(
        &mut self,
        l: Option<CTId>,
        r: Option<CTId>,
        t: &mut VarValues,
    ) -> bool {
        if !self.uniformize(l, r, t) {
            return false;
        }
        self.check_nat_vals(t)
    }

    /// `reduce_type`.
    fn reduce_type(&mut self, a: CTId, t: TypeId) -> CTId {
        assert!(
            self.ct[a].type_len == self.types[t].params_num,
            "reduce_type arity"
        );
        assert!(
            self.ct[a].type_flags == self.types[t].params_types,
            "reduce_type params"
        );
        if self.ct[a].type_len == self.types[t].params_num {
            self.ct[a].act = Act::Type;
            self.ct[a].ty = CType::Type;
            self.ct[a].left = None;
            self.ct[a].right = None;
            self.ct[a].data = CTData::Type(t);
            return a;
        }
        let left = self.ct[a].left.expect("reduce_type without left");
        let l = self.reduce_type(left, t);
        self.ct[a].left = Some(l);
        a
    }

    /// `change_value_var`.
    fn change_value_var(&mut self, o: Option<CTId>, x: &mut VarValues) -> ChangeRes {
        let Some(mut o) = o else {
            return ChangeRes::Null;
        };
        while self.ct[o].act == Act::Var {
            if Self::get_var_value(x, &self.ct[o].data).is_none() {
                break;
            }
            if self.ct[o].ty == CType::Type {
                let dup = Self::get_var_value(x, &self.ct[o].data).unwrap();
                o = self.tree_dup(Some(dup)).unwrap();
            } else {
                let n = Self::get_var_value_num(x, &self.ct[o].data);
                let tv = Self::get_var_value(x, &self.ct[o].data).unwrap();
                self.ct[o].data = self.ct[tv].data.clone();
                self.ct[o].ty = self.ct[tv].ty;
                self.ct[o].act = self.ct[tv].act;
                self.ct[o].type_flags += n + self.ct[tv].type_flags;
            }
        }
        if self.ct[o].act == Act::Field && Self::get_var_value(x, &CTData::Var(o)).is_some() {
            return ChangeRes::Splice;
        }
        let t_left = self.change_value_var(self.ct[o].left, x);
        if t_left == ChangeRes::Err {
            return ChangeRes::Err;
        }
        if t_left == ChangeRes::Splice {
            return match self.change_value_var(self.ct[o].right, x) {
                ChangeRes::Err => ChangeRes::Err,
                ChangeRes::Splice | ChangeRes::Null => ChangeRes::Splice,
                ChangeRes::Node(n) => ChangeRes::Node(n),
            };
        }
        if let ChangeRes::Node(n) = t_left {
            self.ct[o].left = Some(n);
        }
        let t_right = self.change_value_var(self.ct[o].right, x);
        if t_right == ChangeRes::Err {
            return ChangeRes::Err;
        }
        if t_right == ChangeRes::Splice {
            return match self.ct[o].left {
                Some(l) => ChangeRes::Node(l),
                None => ChangeRes::Err,
            };
        }
        if let ChangeRes::Node(n) = t_right {
            self.ct[o].right = Some(n);
        }
        ChangeRes::Node(o)
    }

    /// `tl_tree_lookup_value`.
    fn tree_lookup_value(&self, l: Option<CTId>, var: &CTData, t: &VarValues) -> isize {
        let Some(l) = l else { return -1 };
        if self.ct[l].act == Act::Var && &self.ct[l].data == var {
            return 0;
        }
        if self.ct[l].act == Act::Var {
            match Self::get_var_value(t, &self.ct[l].data) {
                Some(e) => return self.tree_lookup_value(Some(e), var, t),
                None => return -1,
            }
        }
        if self.tree_lookup_value(self.ct[l].left, var, t) >= 0 {
            return 1;
        }
        if self.tree_lookup_value(self.ct[l].right, var, t) >= 0 {
            return 1;
        }
        -1
    }

    /// `tl_tree_lookup_value_nat`.
    fn tree_lookup_value_nat(&self, l: CTId, var: &CTData, x: i64, t: &VarValues) -> isize {
        if self.ct[l].ty == CType::NumValue {
            return -1;
        }
        assert!(self.ct[l].ty == CType::Num && self.ct[l].act == Act::Var);
        if &self.ct[l].data == var {
            return if x == self.ct[l].type_flags { 0 } else { 1 };
        }
        if Self::get_var_value(t, &self.ct[l].data).is_none() {
            return -1;
        }
        let next = Self::get_var_value(t, &self.ct[l].data).unwrap();
        let add = Self::get_var_value_num(t, &self.ct[l].data);
        self.tree_lookup_value_nat(next, var, x + add, t)
    }

    /// `uniformize`.
    fn uniformize(&mut self, l: Option<CTId>, r: Option<CTId>, t: &mut VarValues) -> bool {
        let (Some(mut l), Some(mut r)) = (l, r) else {
            assert!(l.is_none() && r.is_none());
            return true;
        };
        if self.ct[r].act == Act::Var {
            std::mem::swap(&mut l, &mut r);
        }
        if self.ct[l].ty == CType::Type {
            if self.ct[r].ty != CType::Type
                || self.ct[l].type_len != self.ct[r].type_len
                || self.ct[l].type_flags != self.ct[r].type_flags
            {
                return false;
            }
            if self.ct[r].data == CTData::Dummy || self.ct[l].data == CTData::Dummy {
                return true;
            }
            if self.ct[l].act == Act::Var {
                let x = self.tree_lookup_value(Some(r), &self.ct[l].data, t);
                if x > 0 {
                    return false;
                }
                if x == 0 {
                    return true;
                }
                match Self::get_var_value(t, &self.ct[l].data) {
                    None => {
                        if let CTData::Var(f) = self.ct[l].data {
                            Self::set_var_value(t, f, r);
                        }
                        return true;
                    }
                    Some(e) => return self.uniformize(Some(e), Some(r), t),
                }
            }
            if self.ct[l].act != self.ct[r].act || self.ct[l].data != self.ct[r].data {
                return false;
            }
            let (ll, lr) = (self.ct[l].left, self.ct[l].right);
            let (rl, rr) = (self.ct[r].left, self.ct[r].right);
            return self.uniformize(ll, rl, t) && self.uniformize(lr, rr, t);
        }
        assert!(self.ct[l].ty == CType::Num || self.ct[l].ty == CType::NumValue);
        if self.ct[r].ty != CType::Num && self.ct[r].ty != CType::NumValue {
            return false;
        }
        if self.ct[r].data == CTData::Dummy || self.ct[l].data == CTData::Dummy {
            return true;
        }
        let mut x: i64 = 0;
        let mut k = l;
        loop {
            x += self.ct[k].type_flags;
            if self.ct[k].ty == CType::NumValue {
                break;
            }
            if Self::get_var_value(t, &self.ct[k].data).is_none() {
                let s = self.tree_lookup_value_nat(r, &self.ct[k].data, self.ct[k].type_flags, t);
                if s > 0 {
                    return false;
                }
                if s == 0 {
                    return true;
                }
                break;
            }
            x += Self::get_var_value_num(t, &self.ct[k].data);
            k = Self::get_var_value(t, &self.ct[k].data).unwrap();
        }
        let mut y: i64 = 0;
        let mut m = r;
        loop {
            y += self.ct[m].type_flags;
            if self.ct[m].ty == CType::NumValue {
                break;
            }
            if Self::get_var_value(t, &self.ct[m].data).is_none() {
                let s = self.tree_lookup_value_nat(l, &self.ct[m].data, self.ct[m].type_flags, t);
                if s > 0 {
                    return false;
                }
                if s == 0 {
                    return true;
                }
                break;
            }
            y += Self::get_var_value_num(t, &self.ct[m].data);
            m = Self::get_var_value(t, &self.ct[m].data).unwrap();
        }
        let (kd, md) = (self.ct[k].data.clone(), self.ct[m].data.clone());
        if self.ct[k].ty == CType::NumValue && self.ct[m].ty == CType::NumValue {
            return x == y;
        }
        if self.ct[m].ty == CType::NumValue {
            if let CTData::Var(f) = kd {
                Self::set_var_value_num(t, f, m, -(x - y + self.ct[m].type_flags));
            }
            return true;
        }
        if self.ct[k].ty == CType::NumValue {
            if let CTData::Var(f) = md {
                Self::set_var_value_num(t, f, k, -(y - x + self.ct[k].type_flags));
            }
            return true;
        }
        if x >= y {
            if let CTData::Var(f) = kd {
                Self::set_var_value_num(t, f, m, -(x - y + self.ct[m].type_flags));
            }
        } else if let CTData::Var(f) = md {
            Self::set_var_value_num(t, f, k, -(y - x + self.ct[k].type_flags));
        }
        true
    }

    // ----- partial applications -----------------------------------------

    /// `tl_parse_partial_type_app_decl`.
    fn parse_partial_type_app_decl(&mut self, t: &TreeNode) -> bool {
        assert!(t.ty == TreeType::PartialTypeAppDecl && !t.children.is_empty());
        assert!(t.children[0].ty == TreeType::BoxedTypeIdent);
        let Some(ty) = self.get_type(self.text(&t.children[0])) else {
            self.err("Can not make partial app for unknown type");
            return false;
        };
        self.type_finalize(ty);
        let Some(mut l) = self.parse_ident(&t.children[0], 0) else {
            panic!("partial type app head");
        };
        self.buf.clear();
        let nc = t.children.len();
        let cc = nc as i32 - 1;
        for child in &t.children[1..] {
            let Some(z) = self.parse_any_term(child, 0) else {
                return false;
            };
            let Some(nl) = self.union_(Some(l), Some(z)) else {
                return false;
            };
            l = nl;
            self.buf_add_tree(self.ct[l].right, true);
        }
        while self.ct[l].type_len != 0 {
            let c = self.ct_new();
            self.ct[c].act = Act::Var;
            self.ct[c].ty = if self.ct[l].type_flags & 1 != 0 {
                CType::Num
            } else {
                CType::Type
            };
            self.ct[c].type_len = 0;
            self.ct[c].type_flags = 0;
            self.ct[c].data = CTData::Dummy;
            let Some(nl) = self.union_(Some(l), Some(c)) else {
                return false;
            };
            l = nl;
        }
        let new_id = format!(
            "{}{}",
            self.types[ty].id,
            String::from_utf8_lossy(&self.buf)
        );
        let Some(nt) = self.add_type(
            new_id.as_bytes(),
            self.types[ty].params_num - cc,
            self.types[ty].params_types >> cc,
        ) else {
            panic!("partial type app id");
        };
        for i in 0..self.types[ty].constructors.len() {
            let c = self.types[ty].constructors[i];
            let mut v: VarValues = BTreeMap::new();
            let a = self.tree_dup(self.ctors[c].left).expect("ctor left");
            let b = self.tree_dup(self.ctors[c].right).expect("ctor right");
            let mut w: VarValues = BTreeMap::new();
            self.change_var_ptrs(self.ctors[c].left, Some(a), &mut w);
            self.change_var_ptrs(self.ctors[c].right, Some(b), &mut w);
            if !self.check_constructors_equal(Some(b), Some(l), &mut v) {
                continue;
            }
            let b = self.reduce_type(b, nt);
            let ar = self.change_value_var(Some(a), &mut v);
            let a = if ar == ChangeRes::Splice {
                None
            } else {
                ar.node()
            };
            let br = self.change_value_var(Some(b), &mut v);
            assert!(br != ChangeRes::Splice, "splice on right side");
            let b = br.node();
            let rid = format!("{}{}", self.ctors[c].id, String::from_utf8_lossy(&self.buf));
            let Some(r) = self.add_constructor(nt, rid.as_bytes(), true) else {
                return false;
            };
            let real = self.ctors[c].id.clone();
            self.ctors[r].real_id = Some(real);
            self.ctors[r].left = a;
            self.ctors[r].right = b;
            if self.ctors[r].name == 0 {
                self.count_combinator_name(r);
            }
        }
        true
    }

    /// `tl_parse_partial_comb_app_decl`.
    fn parse_partial_comb_app_decl(&mut self, t: &TreeNode, fun: bool) -> bool {
        assert!(t.ty == TreeType::PartialCombAppDecl);
        let c0 = self.text(&t.children[0]);
        let Some(c) = (if !fun {
            self.get_constructor(c0)
        } else {
            self.get_function(c0)
        }) else {
            self.err("Can not make partial app for undefined combinator");
            return false;
        };
        let mut l = self.tree_dup(self.ctors[c].left).expect("ctor left");
        let r = self.tree_dup(self.ctors[c].right).expect("ctor right");
        let mut v: VarValues = BTreeMap::new();
        self.change_var_ptrs(self.ctors[c].left, Some(l), &mut v);
        self.change_var_ptrs(self.ctors[c].right, Some(r), &mut v);
        v.clear();
        self.buf.clear();
        for child in &t.children[1..] {
            let Some(x) = self.parse_any_term(child, 0) else {
                return false;
            };
            let mut k: Option<CTId> = None;
            let z = self.change_first_var(Some(l), &mut k, x);
            let Some(zn) = z.node() else {
                return false;
            };
            l = zn;
            if k.is_none() {
                self.err("Partial app: not enougth variables");
                return false;
            }
            // K stays set for the right-side pass, exactly as in C.
            let z = self.change_first_var(Some(r), &mut k, x);
            let Some(zn) = z.node() else {
                return false;
            };
            assert!(zn == r, "right side replaced");
            self.buf_add_tree(Some(x), true);
        }
        let rid = format!("{}{}", self.ctors[c].id, String::from_utf8_lossy(&self.buf));
        let r2 = if !fun {
            let ty = self.ctors[c].ty.expect("constructor type");
            self.add_constructor(ty, rid.as_bytes(), true)
        } else {
            self.add_function(self.ctors[c].ty, rid.as_bytes(), true)
        };
        let Some(r2) = r2 else { return false };
        let real = self.ctors[c].id.clone();
        self.ctors[r2].real_id = Some(real);
        self.ctors[r2].left = Some(l);
        self.ctors[r2].right = Some(r);
        if self.ctors[r2].name == 0 {
            self.count_combinator_name(r2);
        }
        true
    }

    /// `tl_parse_partial_app_decl`.
    fn parse_partial_app_decl(&mut self, t: &TreeNode, fun: bool) -> bool {
        assert!(t.ty == TreeType::PartialAppDecl && t.children.len() == 1);
        if t.children[0].ty == TreeType::PartialCombAppDecl {
            self.parse_partial_comb_app_decl(&t.children[0], fun)
        } else {
            if fun {
                self.err("Partial type app in functions block");
                return false;
            }
            self.parse_partial_type_app_decl(&t.children[0])
        }
    }

    // ----- final and builtin declarations -------------------------------

    /// `tl_parse_final_final`.
    fn parse_final_final(&mut self, t: &TreeNode) -> bool {
        assert!(t.ty == TreeType::FinalFinal && t.children.len() == 1);
        match self.get_type(self.text(&t.children[0])) {
            Some(ty) => {
                self.types[ty].flags |= TYPE_FINAL;
                true
            }
            None => {
                self.err(format!(
                    "Final statement for type `{}` before declaration",
                    String::from_utf8_lossy(self.text(&t.children[0]))
                ));
                false
            }
        }
    }

    /// `tl_parse_final_new`.
    fn parse_final_new(&mut self, t: &TreeNode) -> bool {
        assert!(t.ty == TreeType::FinalNew && t.children.len() == 1);
        if self.get_type(self.text(&t.children[0])).is_some() {
            self.err(format!(
                "New statement: type `{}` already declared",
                String::from_utf8_lossy(self.text(&t.children[0]))
            ));
            false
        } else {
            true
        }
    }

    /// `tl_parse_final_empty`.
    fn parse_final_empty(&mut self, t: &TreeNode) -> bool {
        assert!(t.ty == TreeType::FinalEmpty && t.children.len() == 1);
        if self.get_type(self.text(&t.children[0])).is_some() {
            self.err(format!(
                "New statement: type `{}` already declared",
                String::from_utf8_lossy(self.text(&t.children[0]))
            ));
            return false;
        }
        let Some(ty) = self.add_type(self.text(&t.children[0]), 0, 0) else {
            panic!("Empty type");
        };
        self.types[ty].flags |= TYPE_FINAL | TYPE_EMPTY;
        true
    }

    /// `tl_parse_final_decl`.
    fn parse_final_decl(&mut self, t: &TreeNode, fun: bool) -> bool {
        assert!(t.ty == TreeType::FinalDecl && t.children.len() == 1 && !fun);
        match t.children[0].ty {
            TreeType::FinalNew => self.parse_final_new(&t.children[0]),
            TreeType::FinalFinal => self.parse_final_final(&t.children[0]),
            TreeType::FinalEmpty => self.parse_final_empty(&t.children[0]),
            _ => panic!("unexpected final node"),
        }
    }

    /// `tl_parse_builtin_combinator_decl`. Note the C function returns -1
    /// (truthy, i.e. "success") for a builtin in the functions block — an
    /// error-message-only quirk kept as-is.
    fn parse_builtin_combinator_decl(&mut self, t: &TreeNode, fun: bool) -> bool {
        if fun {
            self.err("Builtin type can not be described in function block");
            return true;
        }
        assert!(
            t.ty == TreeType::BuiltinCombinatorDecl
                && t.children.len() == 2
                && t.children[0].ty == TreeType::FullCombinatorId
                && t.children[1].ty == TreeType::BoxedTypeIdent
        );
        const COMBOS: [(&[u8], &[u8]); 6] = [
            (b"int", b"Int"),
            (b"long", b"Long"),
            (b"double", b"Double"),
            (b"object", b"Object"),
            (b"function", b"Function"),
            (b"string", b"String"),
        ];
        let c0 = self.text(&t.children[0]);
        let c1 = self.text(&t.children[1]);
        if COMBOS.iter().any(|&(a, b)| c0 == a && c1 == b) {
            let Some(ty) = self.add_type(c1, 0, 0) else {
                return false;
            };
            let Some(c) = self.add_constructor(ty, c0, false) else {
                return false;
            };
            let left = self.ct_new();
            self.ct[left].act = Act::QuestionMark;
            self.ct[left].ty = CType::ListItem;
            let right = self.ct_new();
            self.ct[right].act = Act::Type;
            self.ct[right].data = CTData::Type(ty);
            self.ct[right].ty = CType::Type;
            self.ctors[c].left = Some(left);
            self.ctors[c].right = Some(right);
            if self.ctors[c].name == 0 {
                self.count_combinator_name(c);
            }
        } else {
            self.err(format!(
                "Unknown builting type `{}`",
                String::from_utf8_lossy(c0)
            ));
            return false;
        }
        true
    }

    /// `tl_parse_declaration`.
    fn parse_declaration(&mut self, t: &TreeNode, fun: bool) -> bool {
        assert!(t.ty == TreeType::Declaration && t.children.len() == 1);
        match t.children[0].ty {
            TreeType::CombinatorDecl => self.parse_combinator_decl(&t.children[0], fun),
            TreeType::PartialAppDecl => self.parse_partial_app_decl(&t.children[0], fun),
            TreeType::FinalDecl => self.parse_final_decl(&t.children[0], fun),
            TreeType::BuiltinCombinatorDecl => {
                self.parse_builtin_combinator_decl(&t.children[0], fun)
            }
            _ => panic!("unexpected declaration node"),
        }
    }

    /// `tl_parse_constr_declarations`.
    fn parse_constr_declarations(&mut self, t: &TreeNode) -> bool {
        assert!(t.ty == TreeType::ConstrDeclarations);
        for child in &t.children {
            if !self.parse_declaration(child, false) {
                return false;
            }
        }
        true
    }

    /// `tl_parse_fun_declarations`.
    fn parse_fun_declarations(&mut self, t: &TreeNode) -> bool {
        assert!(t.ty == TreeType::FunDeclarations);
        for child in &t.children {
            if !self.parse_declaration(child, true) {
                return false;
            }
        }
        true
    }

    // ----- program-level checks -----------------------------------------

    /// `tl_type_check` (the `__ok` flag threaded explicitly; like the C,
    /// once one type fails the remaining checks are skipped).
    fn type_check(&mut self, ok: &mut bool, t: TypeId) {
        if !*ok {
            return;
        }
        if self.types[t].id == "#" {
            self.types[t].name = TYPE_NUM_NAME;
            return;
        }
        if self.types[t].id == "Type" {
            self.types[t].name = TYPE_TYPE_NAME;
            return;
        }
        if self.types[t].constructors.is_empty() && self.types[t].flags & TYPE_EMPTY == 0 {
            self.err(format!("Type {} has no constructors", self.types[t].id));
            *ok = false;
            return;
        }
        let mut name = 0u32;
        for &c in &self.types[t].constructors {
            name ^= self.ctors[c].name;
        }
        self.types[t].name = name;
        let ctors = self.types[t].constructors.clone();
        for i in 0..ctors.len() {
            for j in i + 1..ctors.len() {
                let mut v: VarValues = BTreeMap::new();
                let ri = self.ctors[ctors[i]].right;
                let rj = self.ctors[ctors[j]].right;
                if self.check_constructors_equal(ri, rj, &mut v) {
                    self.types[t].flags |= TYPE_OVERLAPPING;
                }
            }
        }
        if self.types[t].flags & 24 == 24 {
            self.err(format!(
                "Warning: Type {} has overlapping costructors, but it is used with `%`",
                self.types[t].id
            ));
        }
        let mut z = 0;
        let mut sid = 0;
        for (i, &c) in self.types[t].constructors.iter().enumerate() {
            if self.ctors[c].id.starts_with('_') {
                z += 1;
                sid = i;
            }
        }
        if z > 1 {
            self.err(format!(
                "Type {} has {z} default constructors",
                self.types[t].id
            ));
            *ok = false;
            return;
        }
        if z == 1 && self.types[t].flags & TYPE_BARE_USED != 0 {
            self.err(format!(
                "Type {} has default constructors and used bare",
                self.types[t].id
            ));
            *ok = false;
            return;
        }
        if z == 1 {
            let n = self.types[t].constructors.len();
            self.types[t].constructors.swap(sid, n - 1);
        }
    }

    /// `tl_parse`.
    fn tl_parse(&mut self, t: &TreeNode) -> bool {
        assert!(t.ty == TreeType::TlProgram);
        self.add_type(b"#", 0, 0);
        self.add_type(b"Type", 0, 0);
        for child in &t.children {
            let ok = if child.ty == TreeType::ConstrDeclarations {
                self.parse_constr_declarations(child)
            } else {
                self.parse_fun_declarations(child)
            };
            if !ok {
                return false;
            }
        }
        let mut ok = true;
        // In-order treap walk = byte-sorted id order; collect first because
        // `type_check` mutates the tables.
        let order: Vec<TypeId> = self.type_ids.values().copied().collect();
        for tid in order {
            self.type_check(&mut ok, tid);
        }
        ok
    }
}

/// Replaces `.`, `#`, and spaces with `$` — the `print_id` transformation.
fn dollarize(id: &str) -> String {
    id.chars()
        .map(|c| match c {
            '.' | '#' | ' ' => '$',
            c => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> TlProgram {
        TlProgram::parse(src.as_bytes().to_vec()).expect("parse should succeed")
    }

    fn parse_err(src: &str) -> TlParseError {
        TlProgram::parse(src.as_bytes().to_vec()).expect_err("parse should fail")
    }

    fn find_ctor(p: &TlProgram, id: &str) -> usize {
        *p.ctor_ids.get(id).unwrap_or_else(|| panic!("no ctor {id}"))
    }

    fn find_type(p: &TlProgram, id: &str) -> usize {
        *p.type_ids.get(id).unwrap_or_else(|| panic!("no type {id}"))
    }

    #[test]
    fn builtin_and_magicless_crc_ids() {
        // `int ? = Int;` gives the builtin `int` constructor; magic-less
        // `boolFalse`/`boolTrue` get ids from the canonical-string CRC —
        // all three are the well-known wire values.
        let p = parse("int ? = Int;\nboolFalse = Bool;\nboolTrue = Bool;\n");
        let c = find_ctor(&p, "int");
        assert_eq!(p.ctors[c].name, 0xa850_9bda); // crc32("int ? = Int")
        let c = find_ctor(&p, "boolFalse");
        assert_eq!(p.ctors[c].name, 0xbc79_9737);
        let c = find_ctor(&p, "boolTrue");
        assert_eq!(p.ctors[c].name, 0x9972_75b5);
        // The Bool type's name is the XOR of its constructors' names.
        let t = find_type(&p, "Bool");
        assert_eq!(p.types[t].name, 0xbc79_9737 ^ 0x9972_75b5);
        assert_eq!(p.types[t].constructors.len(), 2);
    }

    #[test]
    fn vector_canonical_form_and_array_args() {
        // Covers opt args, the anonymous `#` field (parsed as args4, giving
        // the array its implicit multiplicity), args2 arrays, and a
        // parameterized result type.
        let p = parse("vector#1cb5c415 {t:Type} # [ t ] = Vector t;\n");
        assert_eq!(p.total_ctors, 1);
        let vector = find_type(&p, "Vector");
        assert_eq!(p.types[vector].params_num, 1);
        assert_eq!(p.types[vector].params_types, 0);
        // The explicit magic is kept as-is (no recomputation).
        let c = find_ctor(&p, "vector");
        assert_eq!(p.ctors[c].name, 0x1cb5_c415);
        // A magic-less variant hashes to the same id via the canonical
        // string "vector t:Type # [ t ] = Vector t".
        let p = parse("vector {t:Type} # [ t ] = Vector t;\n");
        let c = find_ctor(&p, "vector");
        assert_eq!(p.ctors[c].name, 0x1cb5_c415);
    }

    #[test]
    fn explicit_multiplicity_prints_star() {
        // `4*[ int ]` keeps the multiplicity as a child of args2, so the
        // canonical string has the `4*` form (id taken from the C oracle).
        let p = parse("int ? = Int;\nint128 4*[ int ] = Int128;\n");
        let c = find_ctor(&p, "int128");
        assert_eq!(p.ctors[c].name, 0x84cc_f7b7); // crc32("int128 4*[ int ] = Int128")
    }

    #[test]
    fn functions_and_parameterized_field_types() {
        // A field of type `vector<int>` resolves `vector` through its
        // single-constructor rule and applies the `<int>` argument.
        let p = parse(
            "int ? = Int;\nvector#1cb5c415 {t:Type} # [ t ] = Vector t;\n\
             foo#1a2b3c4d bar:vector<int> = Res;\n---functions---\ngetFoo#deadbeef q:Res = Res;\n",
        );
        assert_eq!(p.total_funcs, 1);
        let f = *p.func_ids.get("getFoo").unwrap();
        assert_eq!(p.ctors[f].name, 0xdead_beef);
        let ty = p.ctors[f].ty.expect("function type");
        assert_eq!(p.types[ty].id, "Res");
        assert_eq!(p.total_types, 5); // #, Type, Int, Vector, Res
    }

    #[test]
    fn optional_and_default_constructors() {
        // `_` constructors are exempt from the global id table, mark their
        // type as having a default constructor, and get swapped to the end
        // of the type's constructor list by tl_type_check.
        let p = parse("a#11111111 = T;\n_ = T;\n");
        let t = find_type(&p, "T");
        assert_eq!(p.types[t].constructors.len(), 2);
        assert_eq!(
            p.ctors[p.types[t].constructors[0]].id, "a",
            "default ctor moves last"
        );
        assert_eq!(p.ctors[p.types[t].constructors[1]].id, "_");
        assert!(p.ctor_ids.contains_key("a"));
        assert!(!p.ctor_ids.contains_key("_"));
    }

    #[test]
    fn partial_combinator_application() {
        // None of the five current schemes uses partial apps; exercise the
        // machinery with a synthetic one: `plusOne 5;` derives a new
        // constructor of `Pair` from `plusOne`, with real_id `plusOne`.
        let p = parse("int ? = Int;\nplusOne#1a2b3c4d x:# y:# = Pair;\nplusOne 5;\n");
        assert_eq!(p.total_ctors, 3); // int, plusOne, plusOne 5
        let derived = p
            .ctor_ids
            .keys()
            .find(|id| id.starts_with("plusOne "))
            .expect("derived ctor")
            .clone();
        let c = find_ctor(&p, &derived);
        assert_eq!(p.ctors[c].real_id.as_deref(), Some("plusOne"));
        let ty = p.ctors[c].ty.unwrap();
        assert_eq!(p.types[ty].id, "Pair");
        assert!(p.ctors[c].name != 0, "derived id computed via CRC");
    }

    #[test]
    fn final_and_empty_declarations() {
        let p = parse("int ? = Int;\nNew Foo;\nEmpty Bar;\na#12345678 = Foo;\n");
        assert!(p.type_ids.contains_key("Foo"));
        let bar = find_type(&p, "Bar");
        assert!(p.types[bar].flags & TYPE_EMPTY != 0);
        assert_eq!(p.types[bar].constructors.len(), 0);
        // `New` registers nothing, so a repeat `New` stays legal (as in C);
        // `Empty` does register, so a repeat fails.
        parse("New Foo;\nNew Foo;\n");
        assert!(
            parse_err("Empty Foo;\nEmpty Foo;\n")
                .errors
                .iter()
                .any(|e| e.contains("already declared"))
        );
    }

    #[test]
    fn unused_variable_is_rejected() {
        let e = parse_err("int ? = Int;\nfoo#1a2b3c4d {t:Type} x:int = Res;\n");
        assert!(
            e.errors
                .iter()
                .any(|m| m.contains("Not all variables are used in right side"))
        );
    }

    #[test]
    fn duplicate_constructor_is_rejected() {
        let e = parse_err("int ? = Int;\na#11111111 = T;\na#22222222 = U;\n");
        assert!(
            e.errors
                .iter()
                .any(|m| m.contains("Duplicate constructor id"))
        );
    }

    #[test]
    fn unknown_result_type_is_rejected() {
        // Result types are added on demand, but a field referencing an
        // unknown lowercase ident that is neither var nor type fails.
        let e = parse_err("foo#1a2b3c4d x:unknownIdent = Res;\n");
        assert!(e.errors.iter().any(|m| m.contains("Not a type/var ident")));
    }

    #[test]
    fn hash_and_type_meta_types() {
        let p = parse("int ? = Int;\n");
        let num = find_type(&p, "#");
        assert_eq!(p.types[num].name, TYPE_NUM_NAME);
        let ty = find_type(&p, "Type");
        assert_eq!(p.types[ty].name, TYPE_TYPE_NAME);
        // `#` sorts before letters in the byte-sorted type order.
        let ids: Vec<&String> = p.type_ids.keys().collect();
        assert_eq!(ids.first().copied(), Some(&"#".to_string()));
    }
}
