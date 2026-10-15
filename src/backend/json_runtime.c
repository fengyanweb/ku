/* Ku's yyjson adapter. The yyjson document owns only its own DOM/storage;
 * decoded Ku values own independent strings and containers. Dynamic traversal
 * uses fixed-depth stacks, and typed writers share the same document/budget. */
#define KU_JSON_MAX_INPUT_BYTES ((size_t)1000000)
#define KU_JSON_MAX_OUTPUT_BYTES ((size_t)1000000)
#define KU_JSON_MAX_DEPTH ((size_t)32)
#define KU_JSON_MAX_NODES KU_JSON_MAX_OUTPUT_BYTES

typedef struct KuJsonBuild {
  yyjson_mut_doc* doc;
  KuError error;
  size_t bytes;
  size_t nodes;
  const void* ancestors[33];
  KuValueTag ancestor_tags[33];
} KuJsonBuild;

static KuError ku_json_error(const char* code, const char* message) {
  return ku_error_make(
      ku_string_static((const uint8_t*)"json", 4),
      ku_string_static((const uint8_t*)code, strlen(code)),
      ku_string_static((const uint8_t*)message, strlen(message)));
}

static bool ku_json_fail(KuError* error, const char* code, const char* message) {
  if (error && !error->code.ptr) *error = ku_json_error(code, message);
  return false;
}

static KuError ku_json_error_at(const char* message, size_t position) {
  char text[256];
  int length = snprintf(text, sizeof(text), "json parse failed: %s at byte %zu", message, position);
  if (length < 0 || (size_t)length >= sizeof(text)) {
    return ku_json_error("parse_error", message);
  }
  KuString copied = {0};
  KuString temporary = {(uint8_t*)text, (size_t)length, (size_t)length, KU_STRING_OWNED};
  if (!ku_object_string_try_clone(temporary, &copied)) {
    return ku_json_error("out_of_memory", "json allocation failed");
  }
  return ku_error_make(ku_string_static((const uint8_t*)"json", 4),
      ku_string_static((const uint8_t*)"parse_error", 11), copied);
}

static bool ku_json_build_fail(KuJsonBuild* build, const char* message) {
  return ku_json_fail(&build->error, "stringify_error", message);
}

static yyjson_mut_val* ku_json_build_oom(KuJsonBuild* build) {
  ku_json_fail(&build->error, "out_of_memory", "json allocation failed");
  return NULL;
}

static bool ku_json_build_depth(KuJsonBuild* build, size_t depth) {
  return depth <= KU_JSON_MAX_DEPTH
      || ku_json_build_fail(build, "json value nesting is too deep");
}

static bool ku_json_build_bytes(KuJsonBuild* build, size_t count) {
  if (build->bytes > KU_JSON_MAX_OUTPUT_BYTES
      || count > KU_JSON_MAX_OUTPUT_BYTES - build->bytes) {
    return ku_json_build_fail(build, "json.stringify output is too large");
  }
  build->bytes += count;
  return true;
}

static bool ku_json_build_node(KuJsonBuild* build, size_t bytes) {
  if (build->nodes >= KU_JSON_MAX_NODES) {
    return ku_json_build_fail(build, "json.stringify output is too large");
  }
  if (!ku_json_build_bytes(build, bytes)) return false;
  build->nodes++;
  return true;
}

static bool ku_json_build_enter(
    KuJsonBuild* build, const void* identity, KuValueTag tag, size_t depth) {
  if (!ku_json_build_depth(build, depth)) return false;
  for (size_t index = 0; identity && index < depth; index++) {
    if (build->ancestors[index] == identity && build->ancestor_tags[index] == tag) {
      return ku_json_build_fail(build, "json.stringify does not support circular reference");
    }
  }
  build->ancestors[depth] = identity;
  build->ancestor_tags[depth] = tag;
  return true;
}

static void ku_json_build_leave(KuJsonBuild* build, size_t depth) {
  build->ancestors[depth] = NULL;
}

/* Measure the established Ku spelling, including six-byte b/f escapes. UTF-8
 * validity is checked by yyjson's writer; a malformed native string fails as a
 * Result and never reaches a successful KuString. */
static yyjson_mut_val* ku_json_build_string(KuJsonBuild* build, KuString value) {
  if (value.len && !value.ptr) {
    ku_json_build_fail(build, "invalid utf-8 in json string");
    return NULL;
  }
  if (!ku_json_build_node(build, 2)) return NULL;
  for (size_t index = 0; index < value.len; index++) {
    uint8_t byte = value.ptr[index];
    size_t width = byte == '"' || byte == '\\' || byte == '\n'
        || byte == '\r' || byte == '\t' ? 2 : byte < 0x20 ? 6 : 1;
    if (!ku_json_build_bytes(build, width)) return NULL;
  }
  yyjson_mut_val* node = yyjson_mut_strn(build->doc,
      value.ptr ? (const char*)value.ptr : "", value.len);
  return node ? node : ku_json_build_oom(build);
}

static yyjson_mut_val* ku_json_build_array(KuJsonBuild* build, size_t count) {
  if (!ku_json_build_node(build, 2)
      || (count && !ku_json_build_bytes(build, count - 1))) return NULL;
  yyjson_mut_val* node = yyjson_mut_arr(build->doc);
  return node ? node : ku_json_build_oom(build);
}

static yyjson_mut_val* ku_json_build_object(KuJsonBuild* build, size_t count) {
  if (!ku_json_build_node(build, 2)
      || !ku_json_build_bytes(build, count)
      || (count && !ku_json_build_bytes(build, count - 1))) return NULL;
  yyjson_mut_val* node = yyjson_mut_obj(build->doc);
  return node ? node : ku_json_build_oom(build);
}

static bool ku_json_build_array_add(
    KuJsonBuild* build, yyjson_mut_val* array, yyjson_mut_val* value) {
  if (!array || !value) return false;
  return yyjson_mut_arr_append(array, value)
      || ku_json_build_fail(build, "invalid native array");
}

static bool ku_json_build_object_add(
    KuJsonBuild* build, yyjson_mut_val* object, KuString key, yyjson_mut_val* value) {
  if (!object || !value) return false;
  yyjson_mut_val* name = ku_json_build_string(build, key);
  if (!name) return false;
  return yyjson_mut_obj_add(object, name, value)
      || ku_json_build_fail(build, "invalid native object");
}

static yyjson_mut_val* ku_json_write_unsupported(
    KuJsonBuild* build, size_t depth, const char* message) {
  if (ku_json_build_depth(build, depth)) ku_json_build_fail(build, message);
  return NULL;
}

static int ku_json_entry_compare(const void* left_ptr, const void* right_ptr) {
  const KuEntry* left = *(const KuEntry* const*)left_ptr;
  const KuEntry* right = *(const KuEntry* const*)right_ptr;
  size_t shared = left->key.len < right->key.len ? left->key.len : right->key.len;
  int order = shared ? memcmp(left->key.ptr, right->key.ptr, shared) : 0;
  if (order != 0) return order;
  return left->key.len < right->key.len ? -1 : left->key.len > right->key.len ? 1 : 0;
}

typedef struct KuJsonBuildFrame {
  KuValue value;
  yyjson_mut_val* node;
  KuEntry** entries;
  size_t index;
  size_t count;
  size_t depth;
} KuJsonBuildFrame;

static bool ku_json_build_frame(
    KuJsonBuild* build, KuValue value, size_t depth, KuJsonBuildFrame* frame) {
  *frame = (KuJsonBuildFrame){0};
  frame->value = value;
  frame->depth = depth;
  if (!ku_json_build_depth(build, depth)) return false;
  char number[KU_DTOA_TEXT_CAP];
  switch (value.tag) {
    case KU_NULL:
      if (!ku_json_build_node(build, 4)) return false;
      frame->node = yyjson_mut_null(build->doc);
      break;
    case KU_BOOL:
      if (!ku_json_build_node(build, value.as.b ? 4 : 5)) return false;
      frame->node = yyjson_mut_bool(build->doc, value.as.b);
      break;
    case KU_INT: {
      int length = snprintf(number, sizeof(number), "%lld", (long long)value.as.i);
      if (length <= 0 || (size_t)length >= sizeof(number)) {
        return ku_json_build_fail(build, "json integer formatting failed");
      }
      if (!ku_json_build_node(build, (size_t)length)) return false;
      frame->node = yyjson_mut_sint(build->doc, value.as.i);
      break;
    }
    case KU_FLOAT: {
      if (!isfinite(value.as.f)) {
        return ku_json_build_fail(build, "json.stringify does not support non-finite float");
      }
      size_t length = 0;
      if (!ku_dtoa_format_finite(value.as.f, number, sizeof(number), &length)) {
        return ku_json_build_fail(build, "json float formatting failed");
      }
      if (!ku_json_build_node(build, length)) return false;
      /* Only finite formatter output becomes raw JSON, never user strings. */
      frame->node = yyjson_mut_rawncpy(build->doc, number, length);
      break;
    }
    case KU_STR:
      frame->node = ku_json_build_string(build, value.as.s);
      break;
    case KU_ARRAY: {
      KuValueArray* array = value.as.arr;
      if (array && (array->len > array->cap || (array->len && !array->data))) {
        return ku_json_build_fail(build, "invalid native array");
      }
      if (!ku_json_build_enter(build, array, KU_ARRAY, depth)) return false;
      frame->count = array ? array->len : 0;
      frame->node = ku_json_build_array(build, frame->count);
      break;
    }
    case KU_OBJECT: {
      KuObject* object = value.as.o;
      if (object && (object->len > object->cap || (object->cap && !object->entries))) {
        return ku_json_build_fail(build, "invalid native object");
      }
      if (!ku_json_build_enter(build, object, KU_OBJECT, depth)) return false;
      frame->count = object ? object->len : 0;
      frame->node = ku_json_build_object(build, frame->count);
      if (!frame->node) return false;
      if (!frame->count) break;
      if (frame->count > SIZE_MAX / sizeof(KuEntry*)) {
        return ku_json_build_fail(build, "json object is too large");
      }
      frame->entries = (KuEntry**)malloc(frame->count * sizeof(KuEntry*));
      if (!frame->entries) { ku_json_build_oom(build); return false; }
      size_t found = 0;
      for (size_t index = 0; index < object->cap; index++) {
        if (!object->entries[index].used) continue;
        if (found == frame->count) {
          free(frame->entries);
          frame->entries = NULL;
          return ku_json_build_fail(build, "invalid native object");
        }
        KuEntry* entry = &object->entries[index];
        if (entry->key.len && !entry->key.ptr) {
          free(frame->entries);
          frame->entries = NULL;
          return ku_json_build_fail(build, "invalid utf-8 in json string");
        }
        frame->entries[found++] = entry;
      }
      if (found != frame->count) {
        free(frame->entries);
        frame->entries = NULL;
        return ku_json_build_fail(build, "invalid native object");
      }
      qsort(frame->entries, found, sizeof(KuEntry*), ku_json_entry_compare);
      break;
    }
    case KU_FUNCTION:
      return ku_json_build_fail(build, "json.stringify does not support function");
    default:
      return ku_json_build_fail(build, "json.stringify does not support value");
  }
  if (!frame->node) { ku_json_build_oom(build); return false; }
  return true;
}

static yyjson_mut_val* ku_json_build_value(
    KuJsonBuild* build, KuValue value, size_t depth) {
  KuJsonBuildFrame frames[33] = {{0}};
  size_t count = 1;
  if (!ku_json_build_frame(build, value, depth, &frames[0])) return NULL;
  yyjson_mut_val* root = frames[0].node;
  while (count) {
    KuJsonBuildFrame* parent = &frames[count - 1];
    if (parent->index == parent->count) {
      if (parent->value.tag == KU_ARRAY || parent->value.tag == KU_OBJECT) {
        ku_json_build_leave(build, parent->depth);
      }
      free(parent->entries);
      parent->entries = NULL;
      count--;
      continue;
    }
    KuValue child;
    KuString key = {0};
    if (parent->value.tag == KU_ARRAY) {
      child = parent->value.as.arr->data[parent->index];
    } else {
      key = parent->entries[parent->index]->key;
      child = parent->entries[parent->index]->value;
    }
    if (count >= 33 || !ku_json_build_frame(build, child, parent->depth + 1, &frames[count])) {
      if (count >= 33) ku_json_build_fail(build, "json value nesting is too deep");
      goto fail;
    }
    bool attached = parent->value.tag == KU_ARRAY
        ? ku_json_build_array_add(build, parent->node, frames[count].node)
        : ku_json_build_object_add(build, parent->node, key, frames[count].node);
    if (!attached) {
      free(frames[count].entries);
      goto fail;
    }
    parent->index++;
    count++;
  }
  return root;
fail:
  for (size_t index = 0; index < count; index++) free(frames[index].entries);
  return NULL;
}

/* Preserve Ku's existing b/f spellings. Pairwise escape scanning also skips
 * escaped backslashes, so the literal two-character string "\\b" is unchanged. */
static KuResult_str ku_json_build_finish(KuJsonBuild* build, yyjson_mut_val* root) {
  char* text = NULL;
  size_t length = 0;
  if (root && !build->error.code.ptr) {
    yyjson_mut_doc_set_root(build->doc, root);
    yyjson_write_err write_error = {0};
    text = yyjson_mut_write_opts(build->doc, YYJSON_WRITE_LOWERCASE_HEX, NULL, &length, &write_error);
    if (!text) {
      ku_json_fail(&build->error,
          write_error.code == YYJSON_WRITE_ERROR_MEMORY_ALLOCATION ? "out_of_memory" : "stringify_error",
          write_error.code == YYJSON_WRITE_ERROR_MEMORY_ALLOCATION
              ? "json allocation failed" : "invalid utf-8 in json string");
    }
  }
  if (build->doc) yyjson_mut_doc_free(build->doc);
  build->doc = NULL;
  if (!text) {
    if (!build->error.code.ptr) build->error = ku_json_error("stringify_error", "json stringify failed");
    return (KuResult_str){ false, (KuString){0}, ku_error_move(&build->error) };
  }
  size_t extra = 0;
  for (size_t index = 0; index + 1 < length; index++) {
    if (text[index] == '\\') {
      index++;
      if (text[index] == 'b' || text[index] == 'f') extra += 4;
    }
  }
  if (length > KU_JSON_MAX_OUTPUT_BYTES || extra > KU_JSON_MAX_OUTPUT_BYTES - length) {
    free(text);
    return (KuResult_str){ false, (KuString){0},
        ku_json_error("stringify_error", "json.stringify output is too large") };
  }
  if (extra) {
    uint8_t* normalized = (uint8_t*)malloc(length + extra + 1);
    if (!normalized) {
      free(text);
      return (KuResult_str){ false, (KuString){0},
          ku_json_error("out_of_memory", "json allocation failed") };
    }
    size_t written = 0;
    for (size_t index = 0; index < length; index++) {
      uint8_t byte = (uint8_t)text[index];
      if (byte == '\\' && index + 1 < length) {
        uint8_t escape = (uint8_t)text[++index];
        if (escape == 'b' || escape == 'f') {
          memcpy(normalized + written, escape == 'b' ? "\\u0008" : "\\u000c", 6);
          written += 6;
        } else {
          normalized[written++] = '\\';
          normalized[written++] = escape;
        }
      } else normalized[written++] = byte;
    }
    normalized[written] = 0;
    free(text);
    text = (char*)normalized;
    length = written;
  }
  ku_error_drop(&build->error);
  return (KuResult_str){ true,
      (KuString){(uint8_t*)text, length, length + 1, KU_STRING_OWNED}, (KuError){0} };
}

static KuResult_str ku_json_stringify(KuValue value) {
  KuJsonBuild build = {0};
  build.doc = yyjson_mut_doc_new(NULL);
  if (!build.doc) ku_json_build_oom(&build);
  yyjson_mut_val* root = build.doc ? ku_json_build_value(&build, value, 0) : NULL;
  return ku_json_build_finish(&build, root);
}

typedef struct KuJsonDecodeFrame {
  yyjson_val* source;
  KuValue value;
  KuString key;
  yyjson_arr_iter array;
  yyjson_obj_iter object;
  size_t depth;
} KuJsonDecodeFrame;

static bool ku_json_decode_string(yyjson_val* source, KuString* output) {
  size_t length = yyjson_get_len(source);
  KuString borrowed = {(uint8_t*)yyjson_get_str(source), length, length, KU_STRING_OWNED};
  /* Mark only the borrowed descriptor as owned to force a real copy, even for
   * the empty string. The descriptor itself is never dropped. */
  return ku_object_string_try_clone(borrowed, output);
}

static bool ku_json_decode_frame(
    yyjson_val* source, size_t depth, KuJsonDecodeFrame* frame, KuError* error) {
  *frame = (KuJsonDecodeFrame){0};
  frame->source = source;
  frame->depth = depth;
  frame->value = ku_v_null();
  if (depth > KU_JSON_MAX_DEPTH) {
    return ku_json_fail(error, "parse_error", "json nesting is too deep");
  }
  switch (yyjson_get_type(source)) {
    case YYJSON_TYPE_NULL: break;
    case YYJSON_TYPE_BOOL: frame->value = ku_v_bool(yyjson_get_bool(source)); break;
    case YYJSON_TYPE_NUM:
      if (yyjson_is_sint(source)) frame->value = ku_v_int(yyjson_get_sint(source));
      else if (yyjson_is_uint(source)) {
        uint64_t number = yyjson_get_uint(source);
        if (number > (uint64_t)INT64_MAX) {
          return ku_json_fail(error, "parse_error", "invalid json number: integer is outside int64 range");
        }
        frame->value = ku_v_int((int64_t)number);
      } else {
        double number = yyjson_get_real(source);
        if (!isfinite(number)) return ku_json_fail(error, "parse_error", "json number must be finite");
        frame->value = ku_v_float(number);
      }
      break;
    case YYJSON_TYPE_RAW: {
      const char* raw = yyjson_get_raw(source);
      size_t length = yyjson_get_len(source);
      for (size_t index = 0; index < length; index++) {
        if (raw[index] == '.' || raw[index] == 'e' || raw[index] == 'E') {
          return ku_json_fail(error, "parse_error", "json number must be finite");
        }
      }
      return ku_json_fail(error, "parse_error", "invalid json number: integer is outside int64 range");
    }
    case YYJSON_TYPE_STR: {
      KuString string = {0};
      if (!ku_json_decode_string(source, &string)) goto allocation;
      frame->value = ku_v_str(string);
      break;
    }
    case YYJSON_TYPE_ARR: {
      KuValueArray* array = NULL;
      if (!ku_value_array_try_new(0, &array)) goto allocation;
      frame->value = ku_v_array(array);
      frame->array = yyjson_arr_iter_with(source);
      break;
    }
    case YYJSON_TYPE_OBJ: {
      KuObject* object = NULL;
      if (!ku_object_try_new(0, &object)) goto allocation;
      frame->value = ku_v_object(object);
      frame->object = yyjson_obj_iter_with(source);
      break;
    }
    default: return ku_json_fail(error, "parse_error", "invalid json value");
  }
  return true;
allocation:
  return ku_json_fail(error, "out_of_memory", "json allocation failed");
}

static bool ku_json_decode_value(yyjson_val* source, KuValue* output, KuError* error) {
  KuJsonDecodeFrame frames[33] = {{0}};
  size_t count = 1;
  size_t visited = 1;
  *output = ku_v_null();
  if (!ku_json_decode_frame(source, 0, &frames[0], error)) goto fail;
  while (count) {
    KuJsonDecodeFrame* parent = &frames[count - 1];
    yyjson_val* child = NULL;
    if (parent->value.tag == KU_ARRAY) child = yyjson_arr_iter_next(&parent->array);
    else if (parent->value.tag == KU_OBJECT) {
      yyjson_val* key = yyjson_obj_iter_next(&parent->object);
      if (key) {
        if (!ku_json_decode_string(key, &parent->key)) {
          ku_json_fail(error, "out_of_memory", "json allocation failed");
          goto fail;
        }
        child = yyjson_obj_iter_get_val(key);
      }
    }
    if (child) {
      if (count >= 33) {
        ku_json_fail(error, "parse_error", "json nesting is too deep");
        goto fail;
      }
      if (visited >= KU_JSON_MAX_NODES) {
        ku_json_fail(error, "parse_error", "json input has too many values");
        goto fail;
      }
      visited++;
      if (!ku_json_decode_frame(child, parent->depth + 1, &frames[count], error)) goto fail;
      count++;
      continue;
    }
    KuValue value = ku_value_move(&parent->value);
    ku_string_drop(&parent->key);
    count--;
    if (!count) { *output = value; return true; }
    parent = &frames[count - 1];
    bool inserted = parent->value.tag == KU_ARRAY
        ? ku_value_array_try_push_owned(parent->value.as.arr, &value)
        : ku_object_try_set_owned(parent->value.as.o, &parent->key, &value);
    if (!inserted) {
      ku_value_drop(&value);
      ku_json_fail(error, "out_of_memory", "json allocation failed");
      goto fail;
    }
  }
  return false;
fail:
  for (size_t index = 0; index < count; index++) {
    ku_string_drop(&frames[index].key);
    ku_value_drop(&frames[index].value);
  }
  return false;
}

static KuResult_kuvalue ku_json_try_parse(KuString text) {
  if (text.len > KU_JSON_MAX_INPUT_BYTES) {
    return (KuResult_kuvalue){false, ku_v_null(), ku_json_error("parse_error", "json input is too large")};
  }
  if (text.len && !text.ptr) {
    return (KuResult_kuvalue){false, ku_v_null(), ku_json_error("parse_error", "invalid utf-8 in json input")};
  }
  yyjson_read_err read_error = {0};
  yyjson_doc* doc = yyjson_read_opts(text.ptr ? (char*)text.ptr : (char*)"", text.len,
      YYJSON_READ_BIGNUM_AS_RAW, NULL, &read_error);
  if (!doc) {
    const char* message = read_error.code == YYJSON_READ_ERROR_UNEXPECTED_CONTENT
        ? "unexpected trailing characters"
        : read_error.code == YYJSON_READ_ERROR_DEPTH ? "json nesting is too deep"
        : read_error.msg ? read_error.msg : "invalid json";
    size_t start = 0;
    while (start < text.len && (text.ptr[start] == ' ' || text.ptr[start] == '\t'
        || text.ptr[start] == '\r' || text.ptr[start] == '\n')) start++;
    bool numeric = start < text.len && (text.ptr[start] == '-'
        || (text.ptr[start] >= '0' && text.ptr[start] <= '9'));
    size_t digits = start + (start < text.len && text.ptr[start] == '-');
    bool leading_zero = digits + 1 < text.len && text.ptr[digits] == '0'
        && text.ptr[digits + 1] >= '0' && text.ptr[digits + 1] <= '9';
    if (leading_zero && (read_error.code == YYJSON_READ_ERROR_UNEXPECTED_CONTENT
        || read_error.code == YYJSON_READ_ERROR_INVALID_NUMBER)) message = "leading zero in json number";
    else if (numeric && (read_error.code == YYJSON_READ_ERROR_UNEXPECTED_END
        || read_error.code == YYJSON_READ_ERROR_INVALID_NUMBER)) message = "expected digit in json number";
    KuError error = read_error.code == YYJSON_READ_ERROR_MEMORY_ALLOCATION
        ? ku_json_error("out_of_memory", "json allocation failed")
        : ku_json_error_at(message, read_error.pos);
    return (KuResult_kuvalue){false, ku_v_null(), error};
  }
  KuValue value = ku_v_null();
  KuError error = {0};
  bool ok = ku_json_decode_value(yyjson_doc_get_root(doc), &value, &error);
  yyjson_doc_free(doc);
  if (!ok) {
    ku_value_drop(&value);
    if (!error.code.ptr) error = ku_json_error("parse_error", "invalid json");
    return (KuResult_kuvalue){false, ku_v_null(), error};
  }
  return (KuResult_kuvalue){true, value, (KuError){0}};
}

static KuResult_kuvalue ku_json_parse(KuString text) {
  return ku_json_try_parse(text);
}
