/* NULL headers preserves the legacy server response fast path. A readonly
 * projection produces an independent map and never mutates a borrowed owner. */
static void __ku_http_response_oom(void) {
#if defined(KU_HTTP_RESPONSE_SYNC)
  static const uint8_t message[]="http/out_of_memory: response allocation failed";
  __ku_sync_raise_http_json(message,sizeof(message)-1);
#else
  ku_object_hard_fail_oom();
#endif
}
static void ku_drop_struct___ku_http_response(KuStruct___ku_http_response* v) {
  if (!v) return;
  ku_string_drop(&v->content_type); ku_string_drop(&v->body); ku_string_drop(&v->location);
  ku_object_drop(v->headers); *v=(KuStruct___ku_http_response){0};
}
static bool __ku_http_response_try_clone(KuStruct___ku_http_response v,KuStruct___ku_http_response* out) {
  *out=(KuStruct___ku_http_response){.status=v.status};
  if(!ku_object_string_try_clone(v.content_type,&out->content_type)
      || !ku_object_string_try_clone(v.body,&out->body)
      || !ku_object_string_try_clone(v.location,&out->location)
      || (v.headers && !ku_object_try_clone(v.headers,&out->headers))) {
    ku_drop_struct___ku_http_response(out); return false;
  }
  return true;
}
static KuStruct___ku_http_response ku_clone_struct___ku_http_response(KuStruct___ku_http_response v) {
  KuStruct___ku_http_response out={0};
  if(!__ku_http_response_try_clone(v,&out)) __ku_http_response_oom();
  return out;
}
static KuObject* __ku_http_response_headers_read(KuStruct___ku_http_response v) {
  KuObject* map=NULL;
  if(v.headers) { if(!ku_object_try_clone(v.headers,&map)) __ku_http_response_oom(); return map; }
  if(!ku_object_try_new(0,&map)) {__ku_http_response_oom();return NULL;}
  const char* names[2]={"content-type","location"}; KuString values[2]={v.content_type,v.location};
  for(size_t i=0;i<2;i++) if(values[i].len) {
    KuString copied={0};
    if(!ku_object_string_try_clone(values[i],&copied)) goto oom;
    KuValue value=ku_v_str(copied);
    if(!ku_object_try_set_copy_key(map,ku_string_static((const uint8_t*)names[i],strlen(names[i])),&value)) {
      ku_value_drop(&value);goto oom;
    }
  }
  return map;
oom:
  ku_object_drop(map);__ku_http_response_oom();return NULL;
}
static KuObject* __ku_http_response_headers_mut(KuStruct___ku_http_response* v) {
  if(!v->headers) v->headers=__ku_http_response_headers_read(*v);
  return v->headers;
}
static KuValue __ku_http_response_public_value(KuStruct___ku_http_response v) {
  KuObject* map=NULL;KuObject* headers=__ku_http_response_headers_read(v);KuString body={0};
  if(!headers)return ku_v_null();
  if(!ku_object_try_new(0,&map)||!ku_object_string_try_clone(v.body,&body))goto oom;
  KuValue field=ku_v_int(v.status);
  if(!ku_object_try_set_copy_key(map,ku_string_static((const uint8_t*)"status",6),&field))goto oom;
  field=ku_v_object(headers);
  if(!ku_object_try_set_copy_key(map,ku_string_static((const uint8_t*)"headers",7),&field))goto oom;
  headers=NULL;field=ku_v_str(body);
  if(!ku_object_try_set_copy_key(map,ku_string_static((const uint8_t*)"body",4),&field))goto oom;
  body=(KuString){0};return ku_v_object(map);
oom:
  ku_object_drop(map);ku_object_drop(headers);ku_string_drop(&body);__ku_http_response_oom();return ku_v_null();
}

/* Preserve the public object indexing contract for header-map aliases: a
 * missing key without ? is fatal; the optional form uses the existing Result
 * getter. No borrowed input or owner is consumed here. */
static KuValue __ku_http_headers_get(KuObject* headers, KuString key) {
  KuValue result=ku_v_null();
  KuValue* found=headers?ku_object_get(headers,key):NULL;
  if(!found) {
#if defined(KU_HTTP_RESPONSE_SYNC)
    static const uint8_t message[]="object has no requested header key";
    __ku_sync_raise_http_json(message,sizeof(message)-1);
#else
    abort();
#endif
  } else if(!ku_value_try_clone(*found,&result)) __ku_http_response_oom();
  return result;
}
