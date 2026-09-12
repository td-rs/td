//! Port of the `.tlo` serialization stage (`write_types` and its helpers in
//! `td/generate/tl-parser/tl-parser.c`, lines ~2789–3080): emits the parsed
//! scheme as the serialized TL schema format (TL-tl, schema V2 — see
//! <https://core.telegram.org/mtproto/TL-tl>) that `tl-parser -e` writes.
//!
//! Verified byte-identical to the C original for all five schemes via
//! `conformance/tl_oracle/verify_tlo.sh` (the driver calls td's unmodified
//! `write_types` as the oracle). This covers the combinator argument trees,
//! which the S07 text dump did not.
//!
//! Layout notes, faithfully ported:
//!
//! - iteration order: types byte-sorted by id (the C type treap in-order
//!   walk), constructors grouped per type in per-type array order (post
//!   `tl_type_check` default-constructor swap), functions byte-sorted by id
//!   (the C function treap in-order walk);
//! - the schema `time` field is always 0 (`TL_PARSER_NEED_TIME` is not
//!   defined in td's build; upstream keeps the output reproducible);
//! - `wstr` encodes `null` as four zero bytes and a string as a 1-byte
//!   length, the bytes, and zero padding up to a multiple of 4 counting the
//!   length byte;
//! - the var-value map is write-local: it only ever stores the numeric ids
//!   assigned in [`TloWriter::write_args`], so `BTreeMap<CTId, i64>`
//!   replaces the C `struct tree_var_value *` treap keyed by field-node
//!   pointer (variable identity is the field-node id, as in [`program`]).

use std::collections::BTreeMap;

use crate::program::{Act, CTData, CTId, CType, TlProgram};

// Serialized-schema magic constants (`tl-tl.h`).
const TLS_SCHEMA_V2: i32 = 0x3a2f_9be2;
const TLS_TYPE: i32 = 0x12eb_4386;
const TLS_COMBINATOR: i32 = 0x5c0a_1ed5;
const TLS_COMBINATOR_LEFT_BUILTIN: i32 = 0xcd21_1f63u32 as i32;
const TLS_COMBINATOR_LEFT: i32 = 0x4c12_c6d9;
const TLS_COMBINATOR_RIGHT_V2: i32 = 0x2c06_4372;
const TLS_ARG_V2: i32 = 0x29df_e61b;
const TLS_EXPR_TYPE: i32 = 0xecc9_da78u32 as i32;
const TLS_EXPR_NAT: i32 = 0xdcb4_9bd8u32 as i32;
const TLS_NAT_CONST: i32 = 0xdcb4_9bd8u32 as i32;
const TLS_NAT_VAR: i32 = 0x4e8a_14f0;
const TLS_TYPE_VAR: i32 = 0x0142_ceae;
const TLS_ARRAY: i32 = 0xd9fb_20deu32 as i32;
const TLS_TYPE_EXPR: i32 = 0xc186_3d08u32 as i32;

// Flag remapping constants (`tl-parser.h`); only these named bits survive
// `write_field_flags`/`write_type_flags`/`write_var_type_flags`.
const FLAG_BARE: i64 = 1;
const FLAG_OPT_VAR: i64 = 1 << 17;
const FLAG_EXCL: i64 = 1 << 18;
const FLAG_OPT_FIELD: i64 = 1 << 20;
const FLAG_IS_VAR: i64 = 1 << 21;
const FLAG_DEFAULT_CONSTRUCTOR: i64 = 1 << 25;

/// Serializes `p` in the `.tlo` (serialized schema V2) format.
pub fn write_tlo(p: &TlProgram) -> Vec<u8> {
    let mut w = TloWriter {
        p,
        out: Vec::new(),
        vars: BTreeMap::new(),
        last_var: 0,
    };
    w.write_types();
    w.out
}

struct TloWriter<'a> {
    p: &'a TlProgram,
    out: Vec<u8>,
    /// The write-local `tree_var_value` map: field-node id → assigned number.
    vars: BTreeMap<CTId, i64>,
    last_var: i32,
}

impl<'a> TloWriter<'a> {
    fn ct(&self, id: CTId) -> &crate::program::CTNode {
        &self.p.ct[id]
    }

    // ----- primitives (wint/wdata/wstr/wll) ------------------------------

    fn wint(&mut self, a: i32) {
        self.out.extend_from_slice(&a.to_le_bytes());
    }

    fn wll(&mut self, a: i64) {
        self.out.extend_from_slice(&a.to_le_bytes());
    }

    fn wstr(&mut self, s: Option<&str>) {
        let Some(s) = s else {
            self.wint(0);
            return;
        };
        let bytes = s.as_bytes();
        assert!(
            bytes.len() <= 254,
            "String is too big... ({s:?} is {} bytes)",
            bytes.len()
        );
        self.out.push(bytes.len() as u8);
        self.out.extend_from_slice(bytes);
        // The header length byte counts toward the 4-byte alignment.
        let total = bytes.len() + 1;
        let rem = total & 3;
        if rem != 0 {
            self.out.resize(self.out.len() + (4 - rem), 0);
        }
    }

    // ----- flags ----------------------------------------------------------

    fn write_type_flags(&mut self, flags: i64) {
        let mut new_flags = 0i32;
        if flags & FLAG_BARE != 0 {
            new_flags |= FLAG_BARE as i32;
        }
        if flags & FLAG_DEFAULT_CONSTRUCTOR != 0 {
            new_flags |= FLAG_DEFAULT_CONSTRUCTOR as i32;
        }
        self.wint(new_flags);
    }

    fn write_field_flags(&mut self, flags: i64) {
        let mut new_flags = 0i32;
        if flags & FLAG_BARE != 0 {
            new_flags |= FLAG_BARE as i32;
        }
        if flags & (1 << 5) != 0 {
            new_flags |= FLAG_OPT_VAR as i32;
        }
        if flags & FLAG_EXCL != 0 {
            new_flags |= FLAG_EXCL as i32;
        }
        if flags & FLAG_OPT_FIELD != 0 {
            new_flags |= 2;
        }
        if flags & FLAG_IS_VAR != 0 {
            new_flags |= 4;
        }
        self.wint(new_flags);
    }

    fn write_var_type_flags(&mut self, flags: i64) {
        assert!(
            flags & FLAG_BARE == 0,
            "Sorry, bare vars are not (yet ?) supported."
        );
        self.wint(0);
    }

    // ----- var values ------------------------------------------------------

    fn tl_set_var_value_num(&mut self, var: CTId, num: i64) {
        self.vars.insert(var, num);
    }

    fn tl_get_var_value_num(&self, var: &CTData) -> i64 {
        match var {
            CTData::Var(f) => self.vars.get(f).copied().unwrap_or(0),
            _ => 0,
        }
    }

    // ----- trees -----------------------------------------------------------

    /// `count_list_size`.
    fn count_list_size(&self, t: CTId) -> i32 {
        match self.ct(t).ty {
            CType::ListItem => 1,
            CType::List => {
                self.count_list_size(self.ct(t).left.expect("list without left"))
                    + self.count_list_size(self.ct(t).right.expect("list without right"))
            }
            _ => panic!("count_list_size on non-list"),
        }
    }

    /// `write_args`: serializes a list of combinator fields.
    fn write_args(&mut self, t: CTId) {
        if self.ct(t).ty == CType::List {
            let (l, r) = (self.ct(t).left, self.ct(t).right);
            self.write_args(l.expect("list without left"));
            self.write_args(r.expect("list without right"));
            return;
        }
        self.wint(TLS_ARG_V2);
        let (act, left, data, flags) = {
            let node = self.ct(t);
            (node.act, node.left, node.data.clone(), node.flags)
        };
        assert!(act == Act::Field, "write_args on non-field");
        let left = left.expect("field without left");
        let name = match &data {
            CTData::Str(s) if s != "_" => Some(s.clone()),
            _ => None,
        };
        self.wstr(name.as_deref());
        let mut f = flags;
        if self.ct(left).act == Act::OptField {
            f |= 1 << 20;
        }
        let is_var_field = self.ct(left).act == Act::Type
            && matches!(
                &self.ct(left).data,
                CTData::Type(ty) if self.p.types[*ty].id == "#" || self.p.types[*ty].id == "Type"
            );
        if is_var_field {
            self.write_field_flags(f | FLAG_IS_VAR);
            self.wint(self.last_var);
            self.last_var += 1;
            self.tl_set_var_value_num(t, (self.last_var - 1) as i64);
        } else {
            self.write_field_flags(f);
        }
        self.write_tree(left, 0);
    }

    /// `write_array`.
    fn write_array(&mut self, t: CTId) {
        self.wint(TLS_ARRAY);
        let (l, r) = (self.ct(t).left, self.ct(t).right);
        self.write_tree(l.expect("array without left"), 0);
        self.write_tree(r.expect("array without right"), 0);
    }

    /// `write_type_rec`: a (possibly applied) type expression.
    fn write_type_rec(&mut self, t: CTId, cc: i32) {
        if self.ct(t).act == Act::Arg {
            let (l, r) = (self.ct(t).left, self.ct(t).right);
            self.write_type_rec(l.expect("arg without left"), cc + 1);
            let right = r.expect("arg without right");
            if self.ct(right).ty == CType::NumValue || self.ct(right).ty == CType::Num {
                self.wint(TLS_EXPR_NAT);
            } else {
                self.wint(TLS_EXPR_TYPE);
            }
            self.write_tree(right, 0);
        } else {
            match self.ct(t).act {
                Act::Var => {
                    assert!(cc == 0, "applied type var");
                    self.wint(TLS_TYPE_VAR);
                    let data = self.ct(t).data.clone();
                    let num = self.tl_get_var_value_num(&data);
                    self.wint(num as i32);
                    let flags = self.ct(t).flags;
                    self.write_var_type_flags(flags);
                }
                Act::Type => {
                    self.wint(TLS_TYPE_EXPR);
                    let ty = match &self.ct(t).data {
                        CTData::Type(ty) => *ty,
                        _ => panic!("act_type without type data"),
                    };
                    let name = self.p.types[ty].name;
                    self.wint(name as i32);
                    let flags = self.ct(t).flags;
                    self.write_type_flags(flags);
                    self.wint(cc);
                }
                _ => panic!("write_type_rec on unexpected act"),
            }
        }
    }

    /// `write_opt_type`: a `var.N?Type` conditional field.
    fn write_opt_type(&mut self, t: CTId) {
        let left = self.ct(t).left.expect("opt field without left");
        let data = self.ct(left).data.clone();
        let num = self.tl_get_var_value_num(&data);
        self.wint(num as i32);
        let bits = self.ct(left).type_flags;
        self.wint(bits as i32);
        let right = self.ct(t).right.expect("opt field without right");
        match self.ct(right).act {
            Act::Array => self.write_array(right),
            Act::Type | Act::Var | Act::Arg => self.write_type_rec(right, 0),
            act => panic!("write_opt_type on unexpected act {act:?}"),
        }
    }

    /// `write_tree`. The `extra` flag only matters for arg lists, where it
    /// prefixes the count with another `TLS_COMBINATOR_RIGHT_V2`; the result
    /// side of a combinator is always a type tree, so it never fires there.
    fn write_tree(&mut self, t: CTId, extra: i32) {
        match self.ct(t).ty {
            CType::ListItem | CType::List => {
                if extra != 0 {
                    self.wint(TLS_COMBINATOR_RIGHT_V2);
                }
                let n = self.count_list_size(t);
                self.wint(n);
                self.write_args(t);
            }
            CType::NumValue => {
                self.wint(TLS_NAT_CONST);
                let v = self.ct(t).type_flags;
                self.wint(v as i32);
            }
            CType::Num => {
                self.wint(TLS_NAT_VAR);
                let v = self.ct(t).type_flags;
                self.wint(v as i32);
                let data = self.ct(t).data.clone();
                let num = self.tl_get_var_value_num(&data);
                self.wint(num as i32);
            }
            CType::Type => match self.ct(t).act {
                Act::Array => self.write_array(t),
                Act::Type | Act::Var | Act::Arg => self.write_type_rec(t, 0),
                Act::OptField => self.write_opt_type(t),
                act => panic!("write_tree on unexpected act {act:?}"),
            },
        }
    }

    // ----- program-level writers -------------------------------------------

    /// `is_builtin_type`.
    fn is_builtin_type(id: &str) -> bool {
        matches!(
            id,
            "int" | "long" | "double" | "string" | "object" | "function"
        )
    }

    /// `write_type`.
    fn write_type(&mut self, t: usize) {
        let ty = &self.p.types[t];
        self.wint(TLS_TYPE);
        self.wint(ty.name as i32);
        let id = ty.id.clone();
        self.wstr(Some(&id));
        self.wint(ty.constructors.len() as i32);
        self.wint(ty.flags);
        self.wint(ty.params_num);
        self.wll(ty.params_types);
    }

    /// `write_combinator`.
    fn write_combinator(&mut self, c: usize) {
        let ctor = &self.p.ctors[c];
        self.wint(ctor.name as i32);
        let id = ctor.id.clone();
        self.wstr(Some(&id));
        let type_name = ctor.ty.map(|t| self.p.types[t].name as i32).unwrap_or(0);
        self.wint(type_name);
        self.vars.clear();
        self.last_var = 0;
        if let Some(left) = ctor.left {
            if Self::is_builtin_type(&ctor.id) {
                self.wint(TLS_COMBINATOR_LEFT_BUILTIN);
            } else {
                self.wint(TLS_COMBINATOR_LEFT);
                self.write_tree(left, 0);
            }
        } else {
            self.wint(TLS_COMBINATOR_LEFT);
            self.wint(0);
        }
        self.wint(TLS_COMBINATOR_RIGHT_V2);
        let right = ctor.right.expect("combinator without right");
        self.write_tree(right, 1);
    }

    /// `write_types`.
    fn write_types(&mut self) {
        self.wint(TLS_SCHEMA_V2);
        self.wint(0);
        // Schema "time": 0 keeps the output reproducible (td does not define
        // TL_PARSER_NEED_TIME).
        self.wint(0);
        self.wint(self.p.total_types as i32);
        for &t in self.p.type_ids.values() {
            self.write_type(t);
        }
        self.wint(self.p.total_ctors as i32);
        for &t in self.p.type_ids.values() {
            let ctors = self.p.types[t].constructors.clone();
            for c in ctors {
                self.wint(TLS_COMBINATOR);
                self.write_combinator(c);
            }
        }
        self.wint(self.p.total_funcs as i32);
        for &f in self.p.func_ids.values() {
            self.wint(TLS_COMBINATOR);
            self.write_combinator(f);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> TlProgram {
        TlProgram::parse(src.as_bytes().to_vec()).expect("parse should succeed")
    }

    fn wstr_bytes(s: Option<&str>) -> Vec<u8> {
        let mut w = TloWriter {
            p: &parse("int ? = Int;"),
            out: Vec::new(),
            vars: BTreeMap::new(),
            last_var: 0,
        };
        w.wstr(s);
        w.out
    }

    #[test]
    fn wstr_padding() {
        // null → four zero bytes.
        assert_eq!(wstr_bytes(None), vec![0, 0, 0, 0]);
        // len 1: length byte + 'a' = 2 → pad 2.
        assert_eq!(wstr_bytes(Some("a")), vec![1, b'a', 0, 0]);
        // len 3: 3 + 1 = 4 → no pad.
        assert_eq!(wstr_bytes(Some("abc")), b"\x03abc".to_vec());
        // len 4: 4 + 1 = 5 → pad 3.
        assert_eq!(wstr_bytes(Some("abcd")), b"\x04abcd\x00\x00\x00".to_vec());
        // len 254 is the maximum; 255 aborts in C.
        assert_eq!(wstr_bytes(Some(&"x".repeat(254))).len(), 256);
    }

    #[test]
    fn header_is_magic_then_counts() {
        let p = parse("int ? = Int;\nboolFalse = Bool;\nboolTrue = Bool;\n");
        let out = write_tlo(&p);
        let words: Vec<u32> = out
            .chunks_exact(4)
            .take(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        assert_eq!(
            words,
            vec![TLS_SCHEMA_V2 as u32, 0, 0, p.total_types as u32]
        );
        assert_eq!(p.total_types, 4); // #, Type, Bool, Int
        // The word after the counts opens the first type entry ("#").
        let fifth = u32::from_le_bytes(out[16..20].try_into().unwrap());
        assert_eq!(fifth, TLS_TYPE as u32);
    }

    #[test]
    fn builtin_combinator_uses_left_builtin_marker() {
        let p = parse("int ? = Int;\n");
        let out = write_tlo(&p);
        // Locate the `int` combinator: TLS_COMBINATOR, name crc("int ? = Int")
        // = 0xa8509bda, then the padded "int" string, then the Int type name
        // (same crc), then TLS_COMBINATOR_LEFT_BUILTIN.
        let needle: Vec<u8> = {
            let mut v = Vec::new();
            v.extend_from_slice(&TLS_COMBINATOR.to_le_bytes());
            v.extend_from_slice(&0xa850_9bdau32.to_le_bytes());
            v.extend_from_slice(b"\x03int");
            v.extend_from_slice(&0xa850_9bdau32.to_le_bytes());
            v.extend_from_slice(&TLS_COMBINATOR_LEFT_BUILTIN.to_le_bytes());
            v
        };
        assert!(
            out.windows(needle.len()).any(|w| w == needle),
            "builtin marker not found in {:02x?}",
            out
        );
    }

    #[test]
    fn vector_combinator_serializes_its_var_fields() {
        // The canonical vector: two var-carrying fields (t:Type, the
        // anonymous #) then the array field; the array's multiplicity
        // references the # var's assigned number (1).
        let p = parse("vector#1cb5c415 {t:Type} # [ t ] = Vector t;\n");
        let out = write_tlo(&p);
        // The fields' flag words: t is opt-var + is-var → 1|1<<17|4;
        // # is is-var → 4; the array field carries no flags.
        let needle: Vec<u8> = {
            let mut v = Vec::new();
            v.extend_from_slice(&TLS_ARG_V2.to_le_bytes());
            v.extend_from_slice(b"\x01t\x00\x00"); // "t", padded to 4
            v.extend_from_slice(&(1i32 | (1 << 17) | 4).to_le_bytes());
            v.extend_from_slice(&0i32.to_le_bytes()); // t's var number
            v.extend_from_slice(&TLS_TYPE_EXPR.to_le_bytes());
            v.extend_from_slice(&0x2cec_f817u32.to_le_bytes()); // Type's name
            v.extend_from_slice(&0i32.to_le_bytes()); // type flags, cc=0
            v.extend_from_slice(&0i32.to_le_bytes());
            v
        };
        assert!(
            out.windows(needle.len()).any(|w| w == needle),
            "vector t-field serialization not found"
        );
        // The array multiplicity is TLS_NAT_VAR + 0 + var#1.
        let mult: Vec<u8> = {
            let mut v = Vec::new();
            v.extend_from_slice(&TLS_NAT_VAR.to_le_bytes());
            v.extend_from_slice(&0i32.to_le_bytes());
            v.extend_from_slice(&1i32.to_le_bytes());
            v
        };
        assert!(
            out.windows(mult.len()).any(|w| w == mult),
            "array multiplicity var#1 not found"
        );
    }

    #[test]
    fn optional_field_serializes_var_number_and_bit() {
        // flags:# then flags.0?true — the conditional writes the flags var's
        // assigned number (0) and the bit (0), then the referenced type's
        // expr (`true` resolves through its single constructor).
        let p = parse(
            "true#3fedd339 = True;\nint ? = Int;\n\
             foo#1a2b3c4d flags:# id:Int flags.0?true = Foo;\n",
        );
        let out = write_tlo(&p);
        let needle: Vec<u8> = {
            let mut v = Vec::new();
            v.extend_from_slice(&0i32.to_le_bytes()); // flags var number
            v.extend_from_slice(&0i32.to_le_bytes()); // bit 0
            v.extend_from_slice(&TLS_TYPE_EXPR.to_le_bytes());
            v
        };
        assert!(
            out.windows(needle.len()).any(|w| w == needle),
            "opt-field serialization not found"
        );
    }

    #[test]
    fn output_size_is_word_aligned_per_string_rules_only() {
        // Whole-file sanity: every scheme we can parse round-trips without
        // panicking and grows with its constructor count.
        let small = write_tlo(&parse("int ? = Int;\n"));
        let bigger = write_tlo(&parse(
            "int ? = Int;\nvector#1cb5c415 {t:Type} # [ t ] = Vector t;\n\
             a#11111111 x:vector<int> = T;\n---functions---\n\
             f#22222222 y:vector<T> = Vector T;\n",
        ));
        assert!(bigger.len() > small.len());
    }
}
