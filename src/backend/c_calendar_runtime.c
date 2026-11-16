/* These adapters copy every returned byte before the matching Rust release.
 * They never borrow a Rust-owned view into a Ku value. Inputs remain borrowed. */
static __ku_calendar_slice_v1 __ku_calendar_slice(KuString value) {
  return (__ku_calendar_slice_v1){value.ptr,value.len};
}
static bool __ku_calendar_copy(__ku_calendar_slice_v1 value, KuString* out) {
  *out=(KuString){0};
  if (value.len && !value.data) return false;
  size_t capacity=value.len ? value.len : 1;
  uint8_t* data=(uint8_t*)ku_object_malloc(KU_OBJECT_ALLOC_STRING_CLONE,capacity);
  if (!data) return false;
  if (value.len) memcpy(data,value.data,value.len);
  *out=(KuString){data,value.len,capacity,KU_STRING_OWNED}; return true;
}
static KuError __ku_calendar_static_error(const char* code,const char* message) {
  return ku_error_make(ku_string_static((const uint8_t*)"time",4),
      ku_string_static((const uint8_t*)code,strlen(code)),
      ku_string_static((const uint8_t*)message,strlen(message)));
}
static bool __ku_calendar_put(KuObject* object,const char* key,KuValue* value) {
  return ku_object_try_set_copy_key(object,
      ku_string_static((const uint8_t*)key,strlen(key)),value);
}
typedef struct __ku_calendar_value {
  bool ok; KuObject* object; KuTime time; KuString string;
  int64_t integer; KuError error;
} __ku_calendar_value;
static __ku_calendar_value __ku_calendar_call(__ku_calendar_input_v1 input) {
  __ku_calendar_value value={0};
  __ku_calendar_output_v1 output={0}; output.size=sizeof(output);
  uint32_t status=__ku_calendar_abi_version()==1
      ? __ku_calendar_call_v1(&input,&output) : __ku_calendar_status_invalid_abi;
  if (status!=__ku_calendar_status_ok) {
    if (status==__ku_calendar_status_error) {
      value.error=__ku_calendar_static_error("calendar_error", "calendar conversion failed");
      if (!__ku_calendar_copy(output.code,&value.error.code)
          || !__ku_calendar_copy(output.message,&value.error.message)) {
        ku_error_drop(&value.error);
        value.error=__ku_calendar_static_error("out_of_memory","calendar output allocation failed");
      }
    } else value.error=__ku_calendar_static_error("calendar_error","native calendar ABI failed");
    __ku_calendar_release_v1(&output); return value;
  }
  if (input.op==__ku_calendar_op_date_now || input.op==__ku_calendar_op_date_at
      || input.op==__ku_calendar_op_date_ymd || input.op==__ku_calendar_op_parts) {
    bool parts=input.op==__ku_calendar_op_parts;
    if (!ku_object_try_new(parts?16:8,&value.object)) goto oom;
    KuValue field=ku_v_str(ku_string_static((const uint8_t*)(parts?"time.parts":"time.date"),parts?10:9));
    if (!__ku_calendar_put(value.object,"kind",&field)) goto oom;
    const char* keys[]={"year","month","day","hour","minute","second","millis","weekday"};
    for (size_t i=0;i<(parts?8u:3u);i++) {
      field=ku_v_int(output.numbers[i]);
      if (!__ku_calendar_put(value.object,keys[i],&field)) goto oom;
    }
    if (parts) {
      KuString zone={0}; if (!__ku_calendar_copy(output.text,&zone)) goto oom;
      field=ku_v_str(zone);
      if (!__ku_calendar_put(value.object,"zone",&field)) { ku_value_drop(&field); goto oom; }
    }
  } else if (input.op==__ku_calendar_op_datetime || input.op==__ku_calendar_op_parse) {
    value.time=(KuTime){KU_TIME_TAG,output.numbers[0]};
  } else if (input.op==__ku_calendar_op_format) {
    if (!__ku_calendar_copy(output.text,&value.string)) goto oom;
  } else value.integer=output.numbers[0];
  value.ok=true; __ku_calendar_release_v1(&output); return value;
oom:
  if (value.object) ku_object_drop(value.object);
  value.object=NULL; ku_string_drop(&value.string);
  value.error=__ku_calendar_static_error("out_of_memory","calendar output allocation failed");
  __ku_calendar_release_v1(&output); return value;
}
static void __ku_calendar_fatal(KuError* error) {
  /* The fatal mailbox only retains static bytes; Results retain detailed,
   * owned engine errors. Owner cleanup is handled by the adjacent SyncGuard. */
  const char* message="time/calendar_error: calendar conversion failed";
  if (ku_string_equal(error->code,ku_string_static((const uint8_t*)"invalid_time",12)))
    message="time/invalid_time: time millis outside supported calendar range";
  else if (ku_string_equal(error->code,ku_string_static((const uint8_t*)"invalid_date",12)))
    message="time/invalid_date: invalid calendar date";
  else if (ku_string_equal(error->code,ku_string_static((const uint8_t*)"out_of_memory",13)))
    message="time/out_of_memory: calendar output allocation failed";
  ku_error_drop(error);
  __ku_sync_raise_http_json((const uint8_t*)message,strlen(message));
}
static KuResult_struct___ku_object __ku_calendar_object_result(__ku_calendar_input_v1 input) {
  __ku_calendar_value v=__ku_calendar_call(input);
  return (KuResult_struct___ku_object){v.ok,v.object,v.error};
}
static KuObject* __ku_calendar_object_bare(__ku_calendar_input_v1 input) {
  __ku_calendar_value v=__ku_calendar_call(input);
  if (!v.ok) __ku_calendar_fatal(&v.error); return v.object;
}
static KuResult_time __ku_calendar_time_result(__ku_calendar_input_v1 input) {
  __ku_calendar_value v=__ku_calendar_call(input);
  return (KuResult_time){v.ok,v.time,v.error};
}
static KuResult_str __ku_calendar_str_result(__ku_calendar_input_v1 input) {
  __ku_calendar_value v=__ku_calendar_call(input);
  return (KuResult_str){v.ok,v.string,v.error};
}
static KuString __ku_calendar_str_bare(__ku_calendar_input_v1 input) {
  __ku_calendar_value v=__ku_calendar_call(input);
  if (!v.ok) __ku_calendar_fatal(&v.error); return v.string;
}
static KuResult_int __ku_calendar_int_result(__ku_calendar_input_v1 input) {
  __ku_calendar_value v=__ku_calendar_call(input);
  return (KuResult_int){v.ok,v.integer,v.error};
}
static int64_t __ku_calendar_int_bare(__ku_calendar_input_v1 input) {
  __ku_calendar_value v=__ku_calendar_call(input);
  if (!v.ok) __ku_calendar_fatal(&v.error); return v.integer;
}
static int64_t __ku_calendar_field_int(KuObject* object,KuString key) {
  KuValue* field=ku_object_get(object,key);
  if (!field || field->tag!=KU_INT) {
    KuError error=__ku_calendar_static_error("invalid_date","expected integer calendar field");
    __ku_calendar_fatal(&error); return 0;
  }
  return field->as.i;
}
static KuString __ku_calendar_field_str(KuObject* object,KuString key) {
  KuValue* field=ku_object_get(object,key); KuString copy={0};
  if (!field || field->tag!=KU_STR || !__ku_calendar_copy(__ku_calendar_slice(field->as.s),&copy)) {
    KuError error=__ku_calendar_static_error("invalid_date","expected string calendar field");
    __ku_calendar_fatal(&error);
  }
  return copy;
}
static KuResult_int __ku_calendar_weekday_date_result(KuObject* date,KuString zone,uint32_t argc) {
  __ku_calendar_input_v1 input={0}; input.size=sizeof(input);
  input.op=__ku_calendar_op_weekday_date; input.argc=argc; input.text[0]=__ku_calendar_slice(zone);
  const char* keys[]={"year","month","day"};
  KuValue* kind=ku_object_get(date,ku_string_static((const uint8_t*)"kind",4));
  if (!kind || kind->tag!=KU_STR || !ku_string_equal(kind->as.s,ku_string_static((const uint8_t*)"time.date",9)))
    return (KuResult_int){false,0,__ku_calendar_static_error("invalid_date","expected Date value")};
  for (size_t i=0;i<3;i++) {
    KuValue* field=ku_object_get(date,ku_string_static((const uint8_t*)keys[i],strlen(keys[i])));
    if (!field || field->tag!=KU_INT) return (KuResult_int){false,0,__ku_calendar_static_error("invalid_date","expected integer date field")};
    input.numbers[i]=field->as.i;
  }
  return __ku_calendar_int_result(input);
}
static int64_t __ku_calendar_weekday_date_bare(KuObject* date,KuString zone,uint32_t argc) {
  KuResult_int result=__ku_calendar_weekday_date_result(date,zone,argc);
  if (!result.ok) __ku_calendar_fatal(&result.error); return result.value;
}
