static KuError __ku_http_client_error(const char* code,const char* message) {
  return ku_error_make(ku_string_static((const uint8_t*)"http",4),
    ku_string_static((const uint8_t*)code,strlen(code)),ku_string_static((const uint8_t*)message,strlen(message)));
}
 #if defined(KU_HTTP_CLIENT_SYNC)
static void __ku_http_client_config_error(const char* message) {
  __ku_sync_raise_http_json((const uint8_t*)message,strlen(message));
}
static bool __ku_http_client_key(KuString key,const char* name) {
  return ku_string_equal(key,ku_string_static((const uint8_t*)name,strlen(name)));
}
static int64_t __ku_http_client_config_field_int(KuObject* object,KuString key) {
  KuValue* v=object?ku_object_get(object,key):NULL;
  if(!v||v->tag!=KU_INT){__ku_http_client_config_error("http.client field must remain int");return 0;}
  return v->as.i;
}
static KuString __ku_http_client_config_field_str(KuObject* object,KuString key) {
  KuValue* v=object?ku_object_get(object,key):NULL;KuString result={0};
  if(!v||v->tag!=KU_STR){__ku_http_client_config_error("http.client field must remain str");return result;}
  if(!ku_object_string_try_clone(v->as.s,&result))__ku_http_client_config_error("http/out_of_memory: client field copy failed");
  return result;
}
static bool __ku_http_client_fields(KuObject* object,const char* const* names,size_t count) {
  if(!object)return true;
  for(size_t i=0;i<object->cap;i++)if(object->entries[i].used) {
    bool found=false;
    for(size_t n=0;n<count;n++)if(__ku_http_client_key(object->entries[i].key,names[n])){found=true;break;}
    if(!found){__ku_http_client_config_error("unknown http config field");return false;}
  }
  return true;
}
static bool __ku_http_client_integer(KuObject* object,const char* name,int64_t fallback,int64_t maximum,int64_t* out) {
  KuValue* value=object?ku_object_get(object,ku_string_static((const uint8_t*)name,strlen(name))):NULL;
  if(!value||value->tag==KU_NULL){*out=fallback;return true;}
  if(value->tag!=KU_INT||value->as.i<=0||value->as.i>maximum){__ku_http_client_config_error("http config field must be a positive int within its declared limit");return false;}
  *out=value->as.i;return true;
}
static KuObject* __ku_http_client_config(KuObject* object) {
  static const char* const names[]={"timeout_ms","max_body_bytes","max_idle_connections"};
  int64_t values[3];
  if(!__ku_http_client_fields(object,names,3)
      || !__ku_http_client_integer(object,names[0],5000,300000,&values[0])
      || !__ku_http_client_integer(object,names[1],1000000,16777216,&values[1])
      || !__ku_http_client_integer(object,names[2],128,1024,&values[2]))return NULL;
  KuObject* result=NULL;
  if(!ku_object_try_new(0,&result))goto oom;
  KuValue value=ku_v_str(ku_string_static((const uint8_t*)"http.client",11));
  if(!ku_object_try_set_copy_key(result,ku_string_static((const uint8_t*)"kind",4),&value))goto oom;
  for(size_t i=0;i<3;i++){value=ku_v_int(values[i]);if(!ku_object_try_set_copy_key(result,ku_string_static((const uint8_t*)names[i],strlen(names[i])),&value))goto oom;}
  return result;
oom:ku_object_drop(result);__ku_http_client_config_error("http/out_of_memory: client config allocation failed");return NULL;
}
#endif
#if defined(KU_FEATURE_NATIVE_HTTP_CLIENT)
static bool __ku_http_client_copy(__ku_http_client_slice_v1 value,KuString* out) {
  *out=(KuString){0};if(value.len&&!value.data)return false;
  size_t capacity=value.len?value.len:1;
  uint8_t* data=(uint8_t*)ku_object_malloc(KU_OBJECT_ALLOC_STRING_CLONE,capacity);
  if(!data)return false;if(value.len)memcpy(data,value.data,value.len);
  *out=(KuString){data,value.len,capacity,KU_STRING_OWNED};return true;
}
static uint8_t __ku_http_client_lower(uint8_t c){return c>='A'&&c<='Z'?(uint8_t)(c+32):c;}
static int __ku_http_client_header_compare(const void* a,const void* b) {
  const __ku_http_client_header_v1* x=(const __ku_http_client_header_v1*)a;
  const __ku_http_client_header_v1* y=(const __ku_http_client_header_v1*)b;
  size_t n=x->name.len<y->name.len?x->name.len:y->name.len;
  for(size_t i=0;i<n;i++){uint8_t l=__ku_http_client_lower(x->name.data[i]),r=__ku_http_client_lower(y->name.data[i]);if(l!=r)return l<r?-1:1;}
  return x->name.len<y->name.len?-1:x->name.len!=y->name.len;
}
static bool __ku_http_client_header_eq(KuString s,const char* name) {
  size_t n=strlen(name);if(s.len!=n)return false;
  for(size_t i=0;i<n;i++)if(__ku_http_client_lower(s.ptr[i])!=(uint8_t)name[i])return false;return true;
}
static bool __ku_http_client_token(KuString s) {
  if(!s.len)return false;
  for(size_t i=0;i<s.len;i++){uint8_t c=s.ptr[i];if(!((c>='0'&&c<='9')||(c>='A'&&c<='Z')||(c>='a'&&c<='z')||strchr("!#$%&'*+-.^_`|~",c)&&c))return false;}return true;
}
static bool __ku_http_client_header_value(KuString s) {
  for(size_t i=0;i<s.len;i++)if(s.ptr[i]!=9&&(s.ptr[i]<32||s.ptr[i]==127))return false;return true;
}
/* Prepare once before later config fields, preserving the interpreter's
 * header-before-timeout error priority. The successful caller owns this one
 * descriptor block until execute (or a later config failure) frees it. */
 #if defined(KU_HTTP_CLIENT_SYNC)
static int __ku_http_client_headers(KuObject* headers,__ku_http_client_header_v1** descriptors,size_t* count) {
  *descriptors=NULL;*count=0;
  if(headers&&headers->len){
    if(headers->len>SIZE_MAX/sizeof(*(*descriptors)))goto oom;
    (*descriptors)=(__ku_http_client_header_v1*)ku_object_malloc(KU_OBJECT_ALLOC_ENTRIES,headers->len*sizeof(*(*descriptors)));if(!(*descriptors))goto oom;
    static const char* const managed[]={"host","content-length","transfer-encoding","connection","proxy-connection","keep-alive","te","trailer","upgrade","expect"};
    for(size_t i=0;i<headers->cap;i++)if(headers->entries[i].used){KuEntry* e=&headers->entries[i];
      if(e->value.tag!=KU_STR){__ku_http_client_config_error("http.request header must be str");goto invalid;}
      if(!__ku_http_client_token(e->key)){__ku_http_client_config_error("http.request header name is not a valid HTTP token");goto invalid;}
      if(!__ku_http_client_header_value(e->value.as.s)){__ku_http_client_config_error("http.request header contains control characters");goto invalid;}
      for(size_t n=0;n<sizeof(managed)/sizeof(managed[0]);n++)if(__ku_http_client_header_eq(e->key,managed[n])){__ku_http_client_config_error("http.request header is managed by the HTTP transport");goto invalid;}
      (*descriptors)[(*count)++]=(__ku_http_client_header_v1){{e->key.ptr,e->key.len},{e->value.as.s.ptr,e->value.as.s.len}};
    }
    if(*count>1)qsort((*descriptors),*count,sizeof(*(*descriptors)),__ku_http_client_header_compare);
    for(size_t i=1;i<*count;i++)if(!__ku_http_client_header_compare(&(*descriptors)[i-1],&(*descriptors)[i])){__ku_http_client_config_error("duplicate http.request header");goto invalid;}
  }
  return 1;
invalid:free(*descriptors);*descriptors=NULL;*count=0;return 0;
oom:free(*descriptors);*descriptors=NULL;*count=0;return -1;
}
#endif
static KuResult_struct___ku_http_response __ku_http_client_result(__ku_http_client_output_v1* output,uint32_t status) {
  KuResult_struct___ku_http_response result={0};
  if(status!=0){
    result.error=__ku_http_client_error("transport","native HTTP client failed");
    if(output->code.len&&!__ku_http_client_copy(output->code,&result.error.code))goto oom;
    if(output->message.len&&!__ku_http_client_copy(output->message,&result.error.message))goto oom;
    return result;
  }
  result.value.status=output->http_status;
  if(!__ku_http_client_copy(output->body,&result.value.body)||!ku_object_try_new(output->header_count,&result.value.headers))goto oom;
  for(size_t i=0;i<output->header_count;i++){
    KuString key={0},value={0};
    if(!__ku_http_client_copy(output->headers[i].name,&key)||!__ku_http_client_copy(output->headers[i].value,&value)){ku_string_drop(&key);ku_string_drop(&value);goto oom;}
    KuValue field=ku_v_str(value);
    if(!ku_object_try_set_owned(result.value.headers,&key,&field)){ku_string_drop(&key);ku_value_drop(&field);goto oom;}
  }
  result.ok=true;return result;
oom:
  ku_drop_struct___ku_http_response(&result.value);ku_error_drop(&result.error);result.ok=false;result.error=__ku_http_client_error("out_of_memory","HTTP input or output allocation failed");
  return result;
}
 #if defined(KU_HTTP_CLIENT_SYNC)
static KuResult_struct___ku_http_response __ku_http_client_execute(KuString method,KuString url,KuString body,bool has_body,__ku_http_client_header_v1* descriptors,size_t count,uint64_t timeout,size_t max_body) {
  KuResult_struct___ku_http_response result={0};
  __ku_http_client_output_v1 output={0};output.size=sizeof(output);
  if(__ku_handler_timeout_poll())goto done;
  uint64_t caller_budget=0;
  unsigned long long deadline=__ku_handler_unwind_depth?__ku_handler_cleanup_deadline:__ku_handler_deadline;
  if(deadline){unsigned long long now=__ku_handler_now_ms();if(now>=deadline){(void)__ku_handler_timeout_poll();goto done;}caller_budget=deadline-now;}
  __ku_http_client_input_v1 input={.size=sizeof(input),.flags=has_body?1u:0u,.method={method.ptr,method.len},.url={url.ptr,url.len},.body={body.ptr,body.len},.headers=descriptors,.header_count=count,.timeout_ms=timeout,.caller_budget_ms=caller_budget,.max_body_bytes=max_body};
  uint32_t status=__ku_http_client_abi_version()==1?__ku_http_client_call_v1(&input,&output):2;
  if(__ku_handler_timeout_poll())goto done;
  result=__ku_http_client_result(&output,status);
done:
  __ku_http_client_release_v1(&output);free(descriptors);return result;
}
static KuResult_struct___ku_http_response __ku_http_client_get(KuString url) {
  return __ku_http_client_execute(ku_string_static((const uint8_t*)"GET",3),url,(KuString){0},false,NULL,0,5000,1000000);
}
static KuResult_struct___ku_http_response __ku_http_client_post(KuString url,KuString body) {
  return __ku_http_client_execute(ku_string_static((const uint8_t*)"POST",4),url,body,true,NULL,0,5000,1000000);
}
static KuResult_struct___ku_http_response __ku_http_client_request(KuObject* object) {
  static const char* const names[]={"method","url","headers","body","timeout_ms","max_body_bytes"};
  KuResult_struct___ku_http_response empty={0};
  if(!object||!__ku_http_client_fields(object,names,6)){if(!object)__ku_http_client_config_error("http.request requires an object");return empty;}
  KuValue* value=ku_object_get(object,ku_string_static((const uint8_t*)"url",3));
  if(!value||value->tag!=KU_STR){__ku_http_client_config_error("http.request missing required str url");return empty;}KuString url=value->as.s;
  value=ku_object_get(object,ku_string_static((const uint8_t*)"method",6));KuString method=ku_string_static((const uint8_t*)"GET",3);
  if(value&&value->tag!=KU_NULL){if(value->tag!=KU_STR){__ku_http_client_config_error("http.request method must be str");return empty;}method=value->as.s;}
  value=ku_object_get(object,ku_string_static((const uint8_t*)"body",4));KuString body={0};bool present=false;
  if(value&&value->tag!=KU_NULL){if(value->tag!=KU_STR){__ku_http_client_config_error("http.request body must be str");return empty;}body=value->as.s;present=true;}
  value=ku_object_get(object,ku_string_static((const uint8_t*)"headers",7));KuObject* headers=NULL;
  if(value){if(value->tag!=KU_OBJECT){__ku_http_client_config_error("http.request headers must be object");return empty;}headers=value->as.o;}
  __ku_http_client_header_v1* descriptors=NULL;size_t count=0;
  int prepared=__ku_http_client_headers(headers,&descriptors,&count);
  if(prepared<0){empty.error=__ku_http_client_error("out_of_memory","HTTP input or output allocation failed");return empty;}
  if(!prepared)return empty;
  int64_t timeout,max_body;
  if(!__ku_http_client_integer(object,"timeout_ms",5000,300000,&timeout)||!__ku_http_client_integer(object,"max_body_bytes",1000000,16777216,&max_body)){free(descriptors);return empty;}
  return __ku_http_client_execute(method,url,body,present,descriptors,count,(uint64_t)timeout,(size_t)max_body);
}
#endif
#endif
