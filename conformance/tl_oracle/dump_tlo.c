// S08 oracle: runs td's own tl-parser over a TL scheme file (lexer + parse
// tree + tl_parse) and calls td's own write_types() to stdout, producing the
// serialized schema (.tlo) that the Rust `tl-tlo` bin (crates/tdtl) must
// reproduce byte for byte.
//
// verify_tlo.sh compiles this against the unmodified tl-parser.c/crc32.c
// from the td repository; write_types is declared in tl-parser.h there.
#include <stdio.h>

#include "tl-parser.h"

// Non-static globals tl-parser.c expects its driver to define.
int verbosity;
int output_expressions;
int schema_version;

int main(int argc, char **argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: dump_tlo <scheme.tl>\n");
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
  write_types(stdout);
  return 0;
}
