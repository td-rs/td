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

use tdtl::{Act, CTData, CType, TlProgram};

/// Built-in simple types, mapped by TL type name (`Int32` … `Bool`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prim {
    Int32,
    Int53,
    Int64,
    Double,
    String,
    Bytes,
    Bool,
}

impl Prim {
    /// The Rust type a field of this primitive maps to.
    pub fn rust_type(self) -> &'static str {
        match self {
            Prim::Int32 => "i32",
            Prim::Int53 | Prim::Int64 => "i64",
            Prim::Double => "f64",
            Prim::String => "String",
            Prim::Bytes => "Vec<u8>",
            Prim::Bool => "bool",
        }
    }

    fn by_type_name(name: &str) -> Option<Prim> {
        Some(match name {
            "Int32" => Prim::Int32,
            "Int53" => Prim::Int53,
            "Int64" => Prim::Int64,
            "Double" => Prim::Double,
            "String" => Prim::String,
            "Bytes" => Prim::Bytes,
            "Bool" => Prim::Bool,
            _ => return None,
        })
    }
}

/// The TL type of a field. `Boxed`/`Bare` carry the [`TlProgram`] type id;
/// the distinction is wire-level only (S10) — both become the same Rust type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldType {
    Prim(Prim),
    Vector(Box<FieldType>),
    Boxed(usize),
    Bare(usize),
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
    /// Per type: does it take part in a type cycle (SCC of size ≥ 2 or a
    /// self-reference)? Fields pointing at such types are emitted as
    /// `Box<_>` — the Rust analogue of td's `object_ptr` indirection, which
    /// C++ needs for the very same recursive types.
    pub cyclic: Vec<bool>,
}

impl SchemeModel {
    /// The Rust name of a scheme type.
    pub fn type_name(&self, p: &TlProgram, ty: usize) -> String {
        match &self.type_kinds[ty] {
            TypeKind::Prim(prim) => prim.rust_type().to_owned(),
            TypeKind::Vector => panic!("Vector has no Rust type name"),
            // A single-ctor type is named after the type (the ctor id is its
            // camelCase form); the enum of a multi-ctor type likewise.
            TypeKind::Struct { .. } | TypeKind::Enum { .. } => p.types[ty].id.clone(),
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
            } else if t.id == "#" || t.id == "Type" {
                // Meta types: never referenced by td_api fields directly.
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
        let functions = p.func_ids.values().copied().collect();
        let cyclic = find_cyclic_types(p, &type_kinds, &ctor_fields);
        SchemeModel {
            type_kinds,
            ctor_fields,
            functions,
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
        let mut ty = &f.ty;
        while let FieldType::Vector(inner) = ty {
            ty = inner;
        }
        match ty {
            FieldType::Boxed(t) | FieldType::Bare(t) => out.push(*t),
            FieldType::Prim(_) => {}
            FieldType::Vector(_) => unreachable!(),
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
        walk_fields(p, root, &mut fields);
    }
    fields
}

fn walk_fields(p: &TlProgram, t: usize, out: &mut Vec<Field>) {
    match p.ct[t].ty {
        CType::List => {
            let l = p.ct[t].left.expect("list without left");
            let r = p.ct[t].right.expect("list without right");
            walk_fields(p, l, out);
            walk_fields(p, r, out);
        }
        CType::ListItem => {
            let node = &p.ct[t];
            assert!(node.act == Act::Field, "field list item is not a field");
            let name = match &node.data {
                CTData::Str(s) => s.clone(),
                _ => "_".to_owned(),
            };
            let left = node.left.expect("field without left");
            out.push(Field {
                name,
                ty: field_type(p, left),
            });
        }
        _ => panic!("field list walk hit a non-list node"),
    }
}

/// Maps a field's type tree to a [`FieldType`]. Bare references are the
/// lower-case/single-constructor usages (`vector<textEntity>`, `x:foo`);
/// boxed references are the capitalizedName ones (`chat:Chat`).
fn field_type(p: &TlProgram, node: usize) -> FieldType {
    let n = &p.ct[node];
    match n.act {
        Act::Array => {
            // `# [ t ]` (declaration position): the multiplicity var is the
            // left child, the element list the right one.
            let list = n.right.expect("array without elements");
            assert!(p.ct[list].ty == CType::ListItem);
            let elem = p.ct[list].left.expect("array element field without type");
            let inner = field_type(p, elem);
            FieldType::Vector(Box::new(inner))
        }
        Act::Arg => {
            // `vector<T>` (field position): an applied type whose head is
            // the parameterized Vector type.
            let left = n.left.expect("arg without left");
            assert!(
                p.ct[left].act == Act::Type
                    && matches!(&p.ct[left].data, CTData::Type(t) if p.types[*t].id == "Vector"),
                "only Vector applications appear in field position"
            );
            let right = n.right.expect("arg without right");
            FieldType::Vector(Box::new(field_type(p, right)))
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
        Act::OptField => panic!("conditional fields arrive with the other schemes (S13)"),
        act => panic!("unexpected field type act {act:?}"),
    }
}
