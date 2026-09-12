// Shim replacement for td's test/data.h for the S04 dump oracle.
// Declares the same symbols as the real header but drops the
// td/utils/common.h include, so data.cpp compiles standalone.
// verify.sh stages this file next to a byte-identical copy of test/data.cpp.
#pragma once

#include <cstddef>

extern const char *thumbnail;
extern const size_t thumbnail_size;

extern const char *gzip_bomb;
extern const size_t gzip_bomb_size;

extern const char *gzip;
extern const size_t gzip_size;

extern const char *sqlite_sample_db_v3;
extern const size_t sqlite_sample_db_v3_size;

extern const char *sqlite_sample_db_v4;
extern const size_t sqlite_sample_db_v4_size;
