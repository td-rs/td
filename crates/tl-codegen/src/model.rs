//! The Rust-facing view of a parsed TL scheme: classification of types into
//! primitives / structs / enums and the field lists of every constructor —
//! the `tl_writer_rust` counterpart of td's `tl_writer_td` type mapping
//! (`gen_type_name` / `is_built_in_simple_type` in `td/generate/`).
//!
//! td's C++ writer maps every built-in simple type by its TL name and wraps
//! everything else in `object_ptr<Class>`; boxing (whether the constructor id
//! is written on the wire) is a property of the field, so this model keeps
//! [`FieldType::Boxed`]/[`FieldType::Bare`] distinct even though both map to
//! the same Rust type. td_api has no conditional (`flags.N?`) fields; that
//! extension arrives with the other schemes (S13).

use std::collections::BTreeMap;

use tdtl::{Act, CTData, CTId, CType, TlProgram};

/// td's `gen_class_name`: non-alphanumerics become `_`, case preserved.
pub fn class_name(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Built-in simple types, mapped by TL type name. The union of td_api's
/// (`Int32`…) and telegram/secret/e2e's (`Int`, `Long`, `True`) tables, plus
/// the `#` nat type (the `flags` word), which stores as a plain int32.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prim {
    Nat,
    Int32,
    Int53,
    Int64,
    Double,
    String,
    Bytes,
    Bool,
    /// `True` — the `flags.N?true` pseudo-type: a bool member that is OR'd
    /// into its flags word at store time and never stored on its own.
    True,
}

impl Prim {
    /// The Rust type a field of this primitive maps to.
    pub fn rust_type(self) -> &'static str {
        match self {
            Prim::Nat | Prim::Int32 => "i32",
            Prim::Int53 | Prim::Int64 => "i64",
            Prim::Double => "f64",
            Prim::String => "String",
            Prim::Bytes => "Vec<u8>",
            Prim::Bool | Prim::True => "bool",
        }
    }

    fn by_type_name(name: &str) -> Option<Prim> {
        Some(match name {
            "#" => Prim::Nat,
            "Int" | "Int32" => Prim::Int32,
            "Int53" => Prim::Int53,
            "Long" | "Int64" => Prim::Int64,
            "Double" => Prim::Double,
            "String" => Prim::String,
            "Bytes" => Prim::Bytes,
            "Bool" => Prim::Bool,
            "True" => Prim::True,
            _ => return None,
        })
    }
}

/// The TL type of a field. `Boxed`/`Bare` carry the [`TlProgram`] type id;
/// the distinction is wire-level only (S10) — both become the same Rust type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldType {
    Prim(Prim),
    /// A vector field: bare `vector<t>` (no constructor id on the wire) vs
    /// boxed `Vector<T>` (the `vector#1cb5c415` id precedes the count) —
    /// td's `TlStoreVector` vs `TlStoreBoxed<TlStoreVector, 481674261>`.
    Vector {
        boxed: bool,
        inner: Box<FieldType>,
    },
    /// A fixed-multiplicity array `N*[ T ]` (int128/int256/int512's word
    /// arrays): exactly N elements on the wire with NO count word — td's
    /// `std::array<T, N>`, unlike the variable `vector<T>`.
    FixedArray {
        len: i64,
        inner: Box<FieldType>,
    },
    Boxed(usize),
    Bare(usize),
    /// `flags_var.N?T`: stored only when bit N of the referenced `#` field
    /// (index into the same ctor's field list) is set. `?true` fields
    /// (`inner == Prim::True`) additionally set their bit in the stored word.
    Conditional {
        flags_field: usize,
        bit: i32,
        inner: Box<FieldType>,
    },
    /// A polymorphic `query:!X` type-variable field: any function, stored
    /// boxed (td's `BaseObject::store` dispatch on the var-type arg).
    Function,
}

impl FieldType {
    /// The innermost non-conditional, non-vector type, if any.
    pub fn object_type(&self) -> Option<usize> {
        match self {
            FieldType::Prim(_) | FieldType::Function => None,
            FieldType::Vector { inner, .. }
            | FieldType::FixedArray { inner, .. }
            | FieldType::Conditional { inner, .. } => inner.object_type(),
            FieldType::Boxed(t) | FieldType::Bare(t) => Some(*t),
        }
    }
}

/// One constructor argument (`name:type` in the scheme).
#[derive(Debug, Clone)]
pub struct Field {
    pub name: String,
    pub ty: FieldType,
}

/// What a TL type compiles to in Rust.
#[derive(Debug, Clone)]
pub enum TypeKind {
    /// A built-in simple type — never emitted as a Rust type.
    Prim(Prim),
    /// The parameterized `Vector t` — only appears in field position, as
    /// [`FieldType::Vector`].
    Vector,
    /// A single-constructor type: one Rust struct (named after the type).
    Struct { ctor: usize },
    /// A multi-constructor type: one Rust enum plus one struct per ctor.
    Enum { ctors: Vec<usize> },
}

/// The model of one scheme, indexed exactly like the source [`TlProgram`].
#[derive(Debug)]
pub struct SchemeModel {
    pub type_kinds: Vec<TypeKind>,
    /// Per-ctor fields (same indexing as `TlProgram::ctors`; functions too).
    pub ctor_fields: Vec<Vec<Field>>,
    /// Function ctor ids in byte-sorted id order (the treap walk order).
    pub functions: Vec<usize>,
    /// The first function with no `!X` (polymorphic) fields — the depth-0
    /// sample terminator for the Function umbrella, whose `invokeWithLayer`
    /// style ctors recurse through `Box<Function>`.
    pub safe_function: usize,
    /// Per cyclic type: the constructor whose scalar cyclic children have
    /// minimal breaking rank — the depth-0 sample terminator. `None` for
    /// non-cyclic types (they cannot loop back and use their first ctor).
    pub breaking_ctor: Vec<Option<usize>>,
    /// Per type: does it take part in a type cycle (SCC of size ≥ 2 or a
    /// self-reference)? Fields pointing at such types are emitted as
    /// `Box<_>` — the Rust analogue of td's `object_ptr` indirection, which
    /// C++ needs for the very same recursive types.
    pub cyclic: Vec<bool>,
}

impl SchemeModel {
    /// The Rust name of a scheme type: the id with non-alphanumerics
    /// replaced by `_` (td's `gen_class_name`), case preserved — so Rust
    /// names are one-to-one with the C++ classes.
    pub fn type_name(&self, p: &TlProgram, ty: usize) -> String {
        match &self.type_kinds[ty] {
            TypeKind::Prim(prim) => prim.rust_type().to_owned(),
            TypeKind::Vector => panic!("Vector has no Rust type name"),
            TypeKind::Struct { .. } | TypeKind::Enum { .. } => class_name(&p.types[ty].id),
        }
    }

    /// Builds the model for a parsed scheme.
    pub fn build(p: &TlProgram) -> SchemeModel {
        let mut type_kinds = Vec::with_capacity(p.types.len());
        for t in &p.types {
            let kind = if let Some(prim) = Prim::by_type_name(&t.id) {
                TypeKind::Prim(prim)
            } else if t.id == "Vector" {
                TypeKind::Vector
            } else if t.id == "Type" || t.constructors.is_empty() {
                // The `Type` meta type, never referenced by fields directly.
                TypeKind::Enum { ctors: Vec::new() }
            } else if t.constructors.len() == 1 {
                TypeKind::Struct {
                    ctor: t.constructors[0],
                }
            } else {
                assert!(!t.constructors.is_empty(), "type {} has no ctors", t.id);
                TypeKind::Enum {
                    ctors: t.constructors.clone(),
                }
            };
            type_kinds.push(kind);
        }
        let mut ctor_fields = Vec::with_capacity(p.ctors.len());
        for c in 0..p.ctors.len() {
            ctor_fields.push(fields_of_ctor(p, c));
        }
        let functions: Vec<usize> = p.func_ids.values().copied().collect();
        let cyclic = find_cyclic_types(p, &type_kinds, &ctor_fields);
        // The sample synthesizer (S11) terminates only if every cyclic type
        // has a finite "breaking rank" — computed as a fixed point below.
        // (Cyclic single-ctor types are fine: their scalar cycles pass
        // through an enum, whose manual Default/sample breaks the rank.)
        let breaking_ctor = breaking_ctors(p, &type_kinds, &ctor_fields, &cyclic);
        // Structs derive Default; a scalar cycle that never passes through
        // an enum would make that derivation recurse forever at runtime.
        assert_no_struct_only_cycles(p, &type_kinds, &ctor_fields);
        let safe_function = functions
            .iter()
            .copied()
            .find(|&f| {
                !ctor_fields[f]
                    .iter()
                    .any(|fld| matches!(fld.ty, FieldType::Function))
            })
            .expect("every function has a !X field");
        SchemeModel {
            type_kinds,
            ctor_fields,
            functions,
            safe_function,
            breaking_ctor,
            cyclic,
        }
    }
}

/// The object types each type references directly (through any field depth).
fn type_edges(
    p: &TlProgram,
    type_kinds: &[TypeKind],
    ctor_fields: &[Vec<Field>],
) -> Vec<Vec<usize>> {
    let mut edges = vec![Vec::new(); p.types.len()];
    for (tid, kind) in type_kinds.iter().enumerate() {
        let ctors: Vec<usize> = match kind {
            TypeKind::Prim(_) | TypeKind::Vector => continue,
            TypeKind::Struct { ctor } => vec![*ctor],
            TypeKind::Enum { ctors } => ctors.clone(),
        };
        let mut outs = Vec::new();
        for c in ctors {
            collect_refs(&ctor_fields[c], &mut outs);
        }
        outs.sort_unstable();
        outs.dedup();
        edges[tid] = outs;
    }
    edges
}

fn collect_refs(fields: &[Field], out: &mut Vec<usize>) {
    for f in fields {
        if let Some(t) = f.ty.object_type() {
            out.push(t);
        }
    }
}

/// Tarjan SCC; a type is cyclic if its SCC has ≥ 2 members or it references
/// itself.
fn find_cyclic_types(
    p: &TlProgram,
    type_kinds: &[TypeKind],
    ctor_fields: &[Vec<Field>],
) -> Vec<bool> {
    let mut scc = Tarjan {
        edges: &type_edges(p, type_kinds, ctor_fields),
        index: vec![usize::MAX; p.types.len()],
        low: vec![0; p.types.len()],
        on_stack: vec![false; p.types.len()],
        stack: Vec::new(),
        next_index: 0,
        cyclic: vec![false; p.types.len()],
    };
    for v in 0..scc.edges.len() {
        if scc.index[v] == usize::MAX {
            scc.strongconnect(v);
        }
    }
    scc.cyclic
}

struct Tarjan<'a> {
    edges: &'a [Vec<usize>],
    index: Vec<usize>,
    low: Vec<usize>,
    on_stack: Vec<bool>,
    stack: Vec<usize>,
    next_index: usize,
    cyclic: Vec<bool>,
}

impl Tarjan<'_> {
    fn strongconnect(&mut self, v: usize) {
        self.index[v] = self.next_index;
        self.low[v] = self.next_index;
        self.next_index += 1;
        self.stack.push(v);
        self.on_stack[v] = true;
        for &w in &self.edges[v] {
            if self.index[w] == usize::MAX {
                self.strongconnect(w);
                self.low[v] = self.low[v].min(self.low[w]);
            } else if self.on_stack[w] {
                self.low[v] = self.low[v].min(self.index[w]);
            }
        }
        if self.low[v] == self.index[v] {
            let mut scc = Vec::new();
            while let Some(w) = self.stack.pop() {
                self.on_stack[w] = false;
                scc.push(w);
                if w == v {
                    break;
                }
            }
            let self_loop = scc.len() == 1 && self.edges[v].contains(&v);
            if scc.len() >= 2 || self_loop {
                for &t in &scc {
                    self.cyclic[t] = true;
                }
            }
        }
    }
}

/// Computes the depth-0 breaking constructor for every cyclic type.
///
/// A ctor's cost is `max(rank(u) + 1)` over its scalar (non-vector) fields
/// referencing cyclic types `u` (0 when it has none); a type's rank is its
/// cheapest ctor's cost. Ranks are a least fixed point over the cyclic
/// types: strictly decreasing along every breaking-ctor scalar edge, which
/// makes the sample synthesizer terminate at depth 0 (non-cyclic scalar
/// detours cannot loop back — that would make them cyclic).
///
/// Panics if any cyclic type keeps rank infinity: such a type has no finite
/// wire value, so no scheme may contain it.
fn breaking_ctors(
    p: &TlProgram,
    type_kinds: &[TypeKind],
    ctor_fields: &[Vec<Field>],
    cyclic: &[bool],
) -> Vec<Option<usize>> {
    let ctors_of = |t: usize| -> Vec<usize> {
        match &type_kinds[t] {
            TypeKind::Struct { ctor } => vec![*ctor],
            TypeKind::Enum { ctors } => ctors.clone(),
            _ => Vec::new(),
        }
    };
    let mut rank = vec![usize::MAX; p.types.len()];
    // Least fixed point of `min over ctors of max(rank[child] + 1)`: ranks
    // start at MAX (unknown) and relax down — breakers hit 0 in round one,
    // chains one step per round, and a pure scalar cycle (no exit) keeps
    // MAX forever and is caught by the assert below.
    let cost_of = |rank: &[usize], c: usize| -> usize {
        let mut cost = 0usize;
        for f in &ctor_fields[c] {
            if let FieldType::Boxed(u) | FieldType::Bare(u) = f.ty
                && cyclic[u]
            {
                cost = cost.max(rank[u].saturating_add(1));
            }
        }
        cost
    };
    for _ in 0..=p.types.len() {
        let mut next = vec![usize::MAX; p.types.len()];
        for t in 0..p.types.len() {
            if !cyclic[t] {
                continue;
            }
            for c in ctors_of(t) {
                let cost = cost_of(&rank, c);
                if cost < next[t] {
                    next[t] = cost;
                }
            }
        }
        if next == rank {
            break;
        }
        rank = next;
    }
    let mut result = vec![None; p.types.len()];
    for t in 0..p.types.len() {
        if !cyclic[t] {
            continue;
        }
        assert!(
            rank[t] != usize::MAX,
            "type {} has no finite sample: every constructor leads back into a scalar cycle",
            p.types[t].id
        );
        result[t] = ctors_of(t).into_iter().min_by_key(|&c| cost_of(&rank, c));
    }
    result
}

/// Panics if any scalar cycle runs through single-constructor types only —
/// their derived `Default` impls would recurse without an enum's breaking
/// ctor to stop them.
fn assert_no_struct_only_cycles(
    p: &TlProgram,
    type_kinds: &[TypeKind],
    ctor_fields: &[Vec<Field>],
) {
    let mut edges: Vec<Vec<usize>> = vec![Vec::new(); p.types.len()];
    for t in 0..p.types.len() {
        let TypeKind::Struct { ctor } = type_kinds[t] else {
            continue;
        };
        for f in &ctor_fields[ctor] {
            if let FieldType::Boxed(u) | FieldType::Bare(u) = f.ty
                && matches!(type_kinds[u], TypeKind::Struct { .. })
            {
                edges[t].push(u);
            }
        }
    }
    fn dfs(v: usize, edges: &[Vec<usize>], color: &mut [u8]) -> Result<(), usize> {
        color[v] = 1;
        for &u in &edges[v] {
            match color[u] {
                0 => dfs(u, edges, color)?,
                1 => return Err(u),
                _ => {}
            }
        }
        color[v] = 2;
        Ok(())
    }
    let mut color = vec![0u8; p.types.len()];
    for t in 0..p.types.len() {
        if color[t] == 0
            && let Err(u) = dfs(t, &edges, &mut color)
        {
            panic!(
                "struct-only scalar cycle: {} reaches {} without an enum",
                p.types[t].id, p.types[u].id
            );
        }
    }
}

/// Walks a combinator's argument list (the `write_args` tree walk, read-only).
fn fields_of_ctor(p: &TlProgram, c: usize) -> Vec<Field> {
    // `int ? = Int`-style builtins carry a QuestionMark tree, not fields;
    // `vector`'s only "field" is the polymorphic `t` type variable.
    if matches!(
        p.ctors[c].id.as_str(),
        "int" | "long" | "double" | "string" | "object" | "function" | "vector"
    ) {
        return Vec::new();
    }
    let mut fields = Vec::new();
    if let Some(root) = p.ctors[c].left {
        let mut ids = BTreeMap::new();
        walk_fields(p, root, &mut fields, &mut ids);
    }
    fields
}

fn walk_fields(p: &TlProgram, t: usize, out: &mut Vec<Field>, ids: &mut BTreeMap<CTId, usize>) {
    match p.ct[t].ty {
        CType::List => {
            let l = p.ct[t].left.expect("list without left");
            let r = p.ct[t].right.expect("list without right");
            walk_fields(p, l, out, ids);
            walk_fields(p, r, out, ids);
        }
        CType::ListItem => {
            let node = &p.ct[t];
            assert!(node.act == Act::Field, "field list item is not a field");
            // `{X:Type …}` braces declare type parameters, not data fields
            // (the C++ store skips them; their flag is FLAG_OPT_VAR).
            if node.flags & 32 != 0 {
                return;
            }
            let name = match &node.data {
                CTData::Str(s) => s.clone(),
                _ => "_".to_owned(),
            };
            let left = node.left.expect("field without left");
            ids.insert(t, out.len());
            out.push(Field {
                name,
                ty: field_type(p, left, ids),
            });
        }
        _ => panic!("field list walk hit a non-list node"),
    }
}

/// Maps a field's type tree to a [`FieldType`]. Bare references are the
/// lower-case/single-constructor usages (`vector<textEntity>`, `x:foo`);
/// boxed references are the capitalizedName ones (`chat:Chat`).
fn field_type(p: &TlProgram, node: usize, ids: &BTreeMap<CTId, usize>) -> FieldType {
    let n = &p.ct[node];
    match n.act {
        Act::Array => {
            // `N*[ t ]` / `# [ t ]` (declaration position): the multiplicity
            // is the left child, the element list the right one. A literal
            // N (Act::NatConst, value in type_flags) is a fixed-size array
            // with no count word on the wire; a nat var keeps the variable
            // vector form (count read from the referenced `#` field).
            let mult = &p.ct[n.left.expect("array without multiplicity")];
            let list = n.right.expect("array without elements");
            assert!(p.ct[list].ty == CType::ListItem);
            let elem = p.ct[list].left.expect("array element field without type");
            let inner = field_type(p, elem, ids);
            if mult.ty == CType::NumValue {
                FieldType::FixedArray {
                    len: mult.type_flags,
                    inner: Box::new(inner),
                }
            } else {
                FieldType::Vector {
                    boxed: false,
                    inner: Box::new(inner),
                }
            }
        }
        Act::Arg => {
            // `vector<T>` / `Vector<T>` (field position): an applied type
            // whose head is the parameterized Vector type. The head carries
            // FLAG_BARE exactly when the scheme wrote it lower-case, which
            // td's C++ writer maps to the unboxed `TlStoreVector` form.
            let left = n.left.expect("arg without left");
            assert!(
                p.ct[left].act == Act::Type
                    && matches!(&p.ct[left].data, CTData::Type(t) if p.types[*t].id == "Vector"),
                "only Vector applications appear in field position"
            );
            let boxed = p.ct[left].flags & 1 == 0;
            let right = n.right.expect("arg without right");
            FieldType::Vector {
                boxed,
                inner: Box::new(field_type(p, right, ids)),
            }
        }
        Act::Type => {
            let ty = match &n.data {
                CTData::Type(ty) => *ty,
                _ => panic!("act_type without type data"),
            };
            if let Some(prim) = Prim::by_type_name(&p.types[ty].id) {
                return FieldType::Prim(prim);
            }
            assert!(p.types[ty].id != "Vector", "bare Vector outside an array");
            if n.flags & 1 != 0 {
                FieldType::Bare(ty)
            } else {
                FieldType::Boxed(ty)
            }
        }
        Act::OptField => {
            // `flags.N?T`: the left child is the `flags` var reference
            // (carrying the bit number), the right one the inner type.
            let left = n.left.expect("opt field without left");
            assert!(p.ct[left].act == Act::Var, "opt field without var");
            let bit = p.ct[left].type_flags as i32;
            let flags_node = match &p.ct[left].data {
                CTData::Var(f) => *f,
                _ => panic!("opt field var without field data"),
            };
            let flags_field = ids
                .get(&flags_node)
                .copied()
                .expect("conditional references an unknown flags field");
            let right = n.right.expect("opt field without right");
            FieldType::Conditional {
                flags_field,
                bit,
                inner: Box::new(field_type(p, right, ids)),
            }
        }
        Act::Var => {
            // `query:!X` — a polymorphic type-variable field.
            assert!(n.ty == CType::Type, "nat var in field position");
            FieldType::Function
        }
        act => panic!("unexpected field type act {act:?}"),
    }
}
