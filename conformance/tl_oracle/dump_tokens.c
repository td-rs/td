// S06 one-off oracle: dumps the token stream produced by td's own tl-parser
// lexer (td/generate/tl-parser/tl-parser.c) for a TL scheme file, in the same
// format as the Rust `tl-token-dump` bin (crates/tdtl).
//
// verify.sh compiles this against the unmodified tl-parser.c/crc32.c from the
// td repository; the lexer globals `parse`/`curch` are non-static there.
#include <stdio.h>

#include "tl-parser.h"

extern struct parse parse;
extern char curch;
extern void load_parse(struct parse _parse);
extern char *parse_lex(void);

int verbosity;
int output_expressions;
int schema_version;

static const char *lex_name(int type) {
  switch (type) {
    case lex_error:
      return "lex_error";
    case lex_char:
      return "lex_char";
    case lex_triple_minus:
      return "lex_triple_minus";
    case lex_uc_ident:
      return "lex_uc_ident";
    case lex_lc_ident:
      return "lex_lc_ident";
    case lex_eof:
      return "lex_eof";
    case lex_final:
      return "lex_final";
    case lex_new:
      return "lex_new";
    case lex_none:
      return "lex_none";
    case lex_num:
      return "lex_num";
    case lex_empty:
      return "lex_empty";
  }
  return "?";
}

static void dump(long idx) {
  printf("#%ld type=%s len=%d flags=%d line=%d line_pos=%d text=", idx, lex_name(parse.lex.type),
         parse.lex.len, parse.lex.flags, parse.line, parse.line_pos);
  if (parse.lex.ptr && parse.lex.ptr != (void *)-1L && parse.lex.len > 0) {
    for (int i = 0; i < parse.lex.len; i++) {
      unsigned char u = (unsigned char)parse.lex.ptr[i];
      if (u >= 0x21 && u <= 0x7e) {
        if (u == '\\') {
          fputs("\\\\", stdout);
        } else {
          putchar(u);
        }
      } else {
        printf("\\x%02x", u);
      }
    }
  } else {
    putchar('-');
  }
  putchar('\n');
}

int main(int argc, char **argv) {
  if (argc != 2) {
    fprintf(stderr, "usage: dump_tokens <scheme.tl>\n");
    return 2;
  }
  struct parse *P = tl_init_parse_file(argv[1]);
  if (P == NULL) {
    return 1;
  }
  // Mirror tl_parse_lex: load_parse sets curch, then one token is pre-fetched.
  load_parse(*P);
  if (parse.lex.type == lex_none) {
    parse_lex();
  }
  long idx = 0;
  while (1) {
    dump(++idx);
    if (parse.lex.type == lex_eof || parse.lex.type == lex_error) {
      break;
    }
    parse_lex();
  }
  return 0;
}
