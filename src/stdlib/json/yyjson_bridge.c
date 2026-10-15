/* Private Rust ABI for yyjson's inline DOM accessors; no Ku values cross FFI. */
#include "yyjson.h"
#include <stdlib.h>

typedef struct KuJsonBridgeError {
    uint32_t code;
    size_t position;
    const char *message;
} KuJsonBridgeError;

yyjson_doc *ku_yyjson_read(const char *text, size_t length, KuJsonBridgeError *error) {
    yyjson_read_err detail;
    yyjson_doc *doc = yyjson_read_opts((char *)(void *)text, length,
        YYJSON_READ_BIGNUM_AS_RAW, NULL, &detail);
    error->code = detail.code;
    error->position = detail.pos;
    error->message = detail.msg;
    return doc;
}
void ku_yyjson_doc_free(yyjson_doc *doc) { yyjson_doc_free(doc); }
const yyjson_val *ku_yyjson_root(const yyjson_doc *doc) { return yyjson_doc_get_root(doc); }
uint32_t ku_yyjson_kind(const yyjson_val *value) {
    if (yyjson_is_null(value)) return 0;
    if (yyjson_is_bool(value)) return 1;
    if (yyjson_is_sint(value)) return 2;
    if (yyjson_is_uint(value)) return 3;
    if (yyjson_is_real(value)) return 4;
    if (yyjson_is_str(value)) return 5;
    if (yyjson_is_arr(value)) return 6;
    if (yyjson_is_obj(value)) return 7;
    return 8;
}
bool ku_yyjson_bool(const yyjson_val *value) { return yyjson_get_bool(value); }
int64_t ku_yyjson_int(const yyjson_val *value) { return yyjson_get_sint(value); }
uint64_t ku_yyjson_uint(const yyjson_val *value) { return yyjson_get_uint(value); }
double ku_yyjson_real(const yyjson_val *value) { return yyjson_get_real(value); }
size_t ku_yyjson_length(const yyjson_val *value) { return yyjson_get_len(value); }
const char *ku_yyjson_string(const yyjson_val *value) { return yyjson_get_str(value); }
const char *ku_yyjson_raw(const yyjson_val *value) { return yyjson_get_raw(value); }
const yyjson_val *ku_yyjson_first(const yyjson_val *value) { return value + 1; }
const yyjson_val *ku_yyjson_next(const yyjson_val *value) { return unsafe_yyjson_get_next(value); }

yyjson_mut_doc *ku_yyjson_builder_new(void) { return yyjson_mut_doc_new(NULL); }
void ku_yyjson_builder_free(yyjson_mut_doc *doc) { yyjson_mut_doc_free(doc); }
yyjson_mut_val *ku_yyjson_null(yyjson_mut_doc *doc) { return yyjson_mut_null(doc); }
yyjson_mut_val *ku_yyjson_make_bool(yyjson_mut_doc *doc, bool value) { return yyjson_mut_bool(doc, value); }
yyjson_mut_val *ku_yyjson_make_int(yyjson_mut_doc *doc, int64_t value) { return yyjson_mut_sint(doc, value); }
yyjson_mut_val *ku_yyjson_make_string(yyjson_mut_doc *doc, const char *value, size_t length) {
    return yyjson_mut_strncpy(doc, value, length);
}
/* Only the adapter's finite f64 formatter supplies raw, valid numeric tokens. */
yyjson_mut_val *ku_yyjson_make_number(yyjson_mut_doc *doc, const char *value, size_t length) {
    return yyjson_mut_rawncpy(doc, value, length);
}
yyjson_mut_val *ku_yyjson_array(yyjson_mut_doc *doc) { return yyjson_mut_arr(doc); }
yyjson_mut_val *ku_yyjson_object(yyjson_mut_doc *doc) { return yyjson_mut_obj(doc); }
bool ku_yyjson_append(yyjson_mut_val *array, yyjson_mut_val *value) { return yyjson_mut_arr_append(array, value); }
bool ku_yyjson_add(yyjson_mut_val *object, yyjson_mut_val *key, yyjson_mut_val *value) {
    return yyjson_mut_obj_add(object, key, value);
}
char *ku_yyjson_write(yyjson_mut_doc *doc, yyjson_mut_val *root, size_t *length, KuJsonBridgeError *error) {
    yyjson_write_err detail;
    char *text;
    yyjson_mut_doc_set_root(doc, root);
    text = yyjson_mut_write_opts(doc, YYJSON_WRITE_LOWERCASE_HEX, NULL, length, &detail);
    error->code = detail.code;
    error->position = 0;
    error->message = detail.msg;
    return text;
}
void ku_yyjson_text_free(void *text) { free(text); }
