// Minimal stand-in for tdutils' cmake-generated td/utils/config.h: every
// optional feature disabled, matching what the generator builds need.
// (Same approach as the S04/S10/S12/S15 oracle config.h stand-ins.)
#pragma once

#define TD_HAVE_OPENSSL 0
#define TD_HAVE_ZLIB 0
#define TD_HAVE_CRC32C 0
#define TD_HAVE_COROUTINES 0
#define TD_HAVE_ABSL 0
#define TD_FD_DEBUG 0
