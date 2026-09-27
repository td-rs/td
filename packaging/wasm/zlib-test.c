#include <stdio.h>
#include <string.h>
#include <zlib.h>
int main(void) {
  printf("zlib version %s, sizeof(z_stream)=%zu\n", zlibVersion(), sizeof(z_stream));
  z_stream s;
  memset(&s, 0, sizeof(s));
  int ret = inflateInit2(&s, 47);
  printf("inflateInit2(47) ret=%d\n", ret);
  if (ret == Z_OK) { inflateEnd(&s); }
  // roundtrip sanity
  unsigned char src[64], comp[128], out[64];
  for (int i = 0; i < 64; i++) src[i] = (unsigned char)i;
  z_stream d; memset(&d, 0, sizeof(d));
  printf("deflateInit ret=%d\n", deflateInit(&d, 6));
  d.next_in = src; d.avail_in = 64; d.next_out = comp; d.avail_out = 128;
  printf("deflate ret=%d\n", deflate(&d, Z_FINISH));
  int clen = 128 - d.avail_out; deflateEnd(&d);
  z_stream e; memset(&e, 0, sizeof(e));
  printf("inflateInit ret=%d\n", inflateInit(&e));
  e.next_in = comp; e.avail_in = clen; e.next_out = out; e.avail_out = 64;
  printf("inflate ret=%d\n", inflate(&e, Z_FINISH));
  inflateEnd(&e);
  printf("roundtrip ok=%d\n", memcmp(src, out, 64) == 0);
  return 0;
}
