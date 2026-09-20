// Minimal stand-in for tdutils' cmake-generated td/utils/config.h, for the
// M.5.4 DialogParticipant oracle: openssl on (the telegram misc.cpp md5
// helper needs it), everything else off. Byte-identical shadow to the
// M.5.3 chat_participant_status_oracle's config.h.
#pragma once

#define TD_HAVE_OPENSSL 1
#define TD_HAVE_ZLIB 0
#define TD_HAVE_CRC32C 0
#define TD_HAVE_COROUTINES 0
#define TD_HAVE_ABSL 0
#define TD_FD_DEBUG 0
