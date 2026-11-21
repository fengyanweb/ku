
static int64_t ku_bytes_len(KuBytes value) {
  if (value.len > (size_t)INT64_MAX) {
    fputs("bytes length is outside Ku int range\n", stderr);
    exit(1);
  }
  return (int64_t)value.len;
}

static KuResult_int ku_bytes_get(KuBytes value, int64_t index) {
  if (index < 0 || (uint64_t)index >= (uint64_t)value.len || !value.ptr) {
    return (KuResult_int){ false, 0,
      ku_bytes_error("index_out_of_bounds", "bytes index is out of bounds") };
  }
  return (KuResult_int){ .ok = true, .value = (int64_t)value.ptr[(size_t)index] };
}
