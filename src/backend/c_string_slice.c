static KuResult_str ku_string_slice(KuString s, int64_t start, int64_t end) {
  if (start < 0 || end < 0) {
    return (KuResult_str){ false, (KuString){0}, ku_error_make(ku_string_static((const uint8_t*)"string", 6), ku_string_static((const uint8_t*)"slice_error", 11), ku_string_static((const uint8_t*)"string.slice indexes must be >= 0", 33)) };
  }
  if (start > end) {
    return (KuResult_str){ false, (KuString){0}, ku_error_make(ku_string_static((const uint8_t*)"string", 6), ku_string_static((const uint8_t*)"slice_error", 11), ku_string_static((const uint8_t*)"string.slice start must be <= end", 33)) };
  }
  size_t clen = ku_string_char_len(s);
  if ((uint64_t)end > (uint64_t)clen) {
    char buf[96]; int n = snprintf(buf, sizeof(buf), "string.slice end %lld out of bounds for length %zu", (long long)end, clen);
    KuString msg = (KuString){0}; if (n > 0) { msg = ku_string_alloc((size_t)n); memcpy(msg.ptr, buf, (size_t)n); }
    return (KuResult_str){ false, (KuString){0}, ku_error_make(ku_string_static((const uint8_t*)"string", 6), ku_string_static((const uint8_t*)"slice_error", 11), msg) };
  }
  size_t bi = 0, ci = 0;
  while (bi < s.len && ci < (size_t)start) { bi++; while (bi < s.len && (s.ptr[bi] & 0xC0) == 0x80) bi++; ci++; }
  size_t bstart = bi;
  while (bi < s.len && ci < (size_t)end) { bi++; while (bi < s.len && (s.ptr[bi] & 0xC0) == 0x80) bi++; ci++; }
  size_t blen = bi - bstart;
  KuString outv = ku_string_alloc(blen);
  if (blen) memcpy(outv.ptr, s.ptr + bstart, blen);
  return (KuResult_str){ .ok = true, .value = outv };
}
