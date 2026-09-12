// S07 one-off oracle: runs td's own tl-parser over a TL scheme file
// (lexer + parse tree + tl_parse) and dumps the resulting program in the
// same format as the Rust `tl-scheme-dump` bin (crates/tdtl).
//
// verify_scheme.sh compiles this against the unmodified tl-parser.c/crc32.c
// from the td repository; the globals it needs are non-static there. The
// treap node layouts below replicate the DEFINE_TREE instantiations inside
// tl-parser.c (pointer members only, so the #pragma pack(4) is a no-op).
#include <stdio.h>

#include "tl-parser.h"

extern int total_types_num;
extern int total_constructors_num;
extern int total_functions_num;

struct tree_tl_type {
  struct tree_tl_type *left, *right;
  struct tl_type *x;
  int y;
};
struct tree_tl_constructor {
  struct tree_tl_constructor *left, *right;
  struct tl_constructor *x;
  int y;
};
extern struct tree_tl_type *tl_type_tree;
extern struct tree_tl_constructor *tl_function_tree;

int verbosity;
int output_expressions;
int schema_version;

static void print_ctor(struct tl_constructor *c) {
  printf("ctor name=%08x id=%s real=%s type=%s\n", c->name, c->id,
         c->real_id ? c->real_id : "-", c->type ? c->type->id : "-");
}

static void print_type(struct tl_type *t) {
  printf("type name=%08x params=%d params_types=%lld flags=%d ctors=%d id=%s print=%s\n",
         t->name, t->params_num, t->params_types, t->flags, t->constructors_num, t->id,
         t->print_id);
  for (int i = 0; i < t->constructors_num; i++) {
    print_ctor(t->constructors[i]);
  }
}

static void walk_types(struct tree_tl_type *T) {
  if (!T) {
    return;
  }
  walk_types(T->left);
  print_type(T->x);
  walk_types(T->right);
}

static void walk_funcs(struct tree_tl_constructor *T) {
  if (!T) {
    return;
  }
  walk_funcs(T->left);
  print_ctor(T->x);
  walk_funcs(T->right);
}

int main(int argc, char **argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: dump_scheme <scheme.tl>\n");
    return 2;
  }
  struct parse *P = tl_init_parse_file(argv[1]);
  if (P == NULL) {
    return 1;
  }
  struct tree *T = tl_parse_lex(P);
  if (T == NULL) {
    fprintf(stderr, "Error in parse:\n");
    tl_print_parse_error();
    return 1;
  }
  if (!tl_parse(T)) {
    fprintf(stderr, "Fail\n");
    return 1;
  }
  printf("types=%d constructors=%d functions=%d\n", total_types_num, total_constructors_num,
         total_functions_num);
  walk_types(tl_type_tree);
  walk_funcs(tl_function_tree);
  return 0;
}
