
#define KU_BYTES_MAX_LENGTH (64ULL * 1024ULL * 1024ULL)

static KuError ku_bytes_error(const char* code, const char* message) {
  return ku_error_make(
      ku_string_static((const uint8_t*)"bytes", 5),
      ku_string_static((const uint8_t*)code, strlen(code)),
      ku_string_static((const uint8_t*)message, strlen(message)));
}

static int ku_bytes_try_copy(const uint8_t* data, size_t len, KuBytes* out) {
  if (!out || (len != 0 && !data) || len > (size_t)KU_BYTES_MAX_LENGTH) return -1;
  *out = (KuBytes){0};
  if (len == 0) return 1;
  uint8_t* copy = (uint8_t*)malloc(len);
  if (!copy) return 0;
  memcpy(copy, data, len);
  *out = (KuBytes){ copy, len, len, KU_BYTES_OWNED };
  return 1;
}

static KuBytes ku_move_bytes(KuBytes* value) {
  if (!value) return (KuBytes){0};
  KuBytes moved = *value;
  *value = (KuBytes){0};
  return moved;
}

static KuBytes ku_clone_bytes(KuBytes value) {
  if (value.storage == KU_BYTES_STATIC) return value;
  KuBytes copy = (KuBytes){0};
  int copied = ku_bytes_try_copy(value.ptr, value.len, &copy);
  if (copied != 1) {
    fputs(copied == 0 ? "bytes clone allocation failed\n" : "invalid bytes clone source\n", stderr);
    exit(1);
  }
  return copy;
}

static void ku_drop_bytes(KuBytes* value) {
  if (!value) return;
  if (value->storage == KU_BYTES_OWNED && value->ptr) free(value->ptr);
  *value = (KuBytes){0};
}

static KuResult_bytes ku_bytes_from_str(KuString text) {
  if ((text.len != 0 && !text.ptr) || text.len > (size_t)KU_BYTES_MAX_LENGTH) {
    return (KuResult_bytes){ false, (KuBytes){0},
      ku_bytes_error("too_large", "bytes input is invalid or exceeds 64 MiB") };
  }
  KuBytes value = (KuBytes){0};
  int copied = ku_bytes_try_copy(text.ptr, text.len, &value);
  if (copied != 1) {
    return (KuResult_bytes){ false, (KuBytes){0},
      ku_bytes_error("out_of_memory", "bytes allocation failed") };
  }
  return (KuResult_bytes){ true, value, (KuError){0} };
}
