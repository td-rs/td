// Minimal stand-in for tdutils' cmake-generated td/utils/config.h, for the
// M.5.3 DialogParticipantStatus oracle: openssl on (the telegram misc.cpp
// md5 helper needs it), everything else off. Same approach as
// conformance/tl_store_oracle's S10 shadow, plus the openssl flip.
#pragma once

#define TD_HAVE_OPENSSL 1
#define TD_HAVE_ZLIB 0
#define TD_HAVE_CRC32C 0
#define TD_HAVE_COROUTINES 0
#define TD_HAVE_ABSL 0
#define TD_FD_DEBUG 0
