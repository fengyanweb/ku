/* Source Task HTTP maps remain plain owned KuObjects, with every allocation
 * prepaid against the active frame/driver ledger. No implicit header map is
 * created for unchanged legacy responses. */
enum { KU_TASK_HTTP_MISSING_HEADER=99u };
static int ku_task_http_header_add(size_t* total,size_t bytes) {
  if(bytes>SIZE_MAX-*total)return 0;*total+=bytes;return 1;
}
static int ku_task_http_header_string(KuString value) {
  return (value.storage==KU_STRING_STATIC && !value.capacity && (!value.len || value.ptr))
      || (value.storage==KU_STRING_OWNED && value.len<=value.capacity
          && (value.ptr || (!value.len && !value.capacity)));
}
static size_t ku_task_http_header_copy_bytes(KuString value) {
  return value.storage==KU_STRING_OWNED && value.ptr ? (value.len?value.len:1u):0u;
}
static uint32_t ku_task_http_header_reserve(KuTaskAdapterHostV1* host,size_t bytes) {
  if(!host || !host->ticket || !host->control
      || host->allocation_host_bytes!=sizeof(*host)
      || host->allocation_host_abi!=KU_TASK_ALLOCATION_HOST_ABI_VERSION
      || !host->allocation_context || !host->reconcile_charge)
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  uint32_t status=host->reconcile_charge(host->allocation_context);
  return status==KU_TASK_DRIVER_OK ? ku_task_driver_grow_charge(host->ticket,bytes):status;
}
static uint32_t ku_task_http_header_failed(KuTaskAdapterHostV1* host) {
  uint32_t status=host->reconcile_charge(host->allocation_context);
  return status==KU_TASK_DRIVER_OK ? KU_TASK_HOST_ALLOCATION_FAILED:status;
}
static uint32_t ku_task_host_http_header_get(KuTaskAdapterHostV1* host,
    const KuStruct___ku_http_response* response,KuString key,KuString* output) {
  if(!response || !output || !ku_task_http_header_string(key))return KU_TASK_DRIVER_INVALID_ARGUMENT;
  KuString found={0};int present=0;
  if(response->headers) {
    KuValue* value=ku_object_get(response->headers,key);
    if(value && value->tag==KU_STR){found=value->as.s;present=1;}
  } else if(ku_string_equal(key,ku_string_static((const uint8_t*)"content-type",12)) && response->content_type.len) {
    found=response->content_type;present=1;
  } else if(ku_string_equal(key,ku_string_static((const uint8_t*)"location",8)) && response->location.len) {
    found=response->location;present=1;
  }
  if(!present)return KU_TASK_HTTP_MISSING_HEADER;
  return ku_task_host_clone_string(host,found,output);
}
/* The caller's key and value remain live, including all rejected operations.
 * Existing maps update in place: at most two strings and one growth table are
 * added, never a clone of the whole map. A new map is published only after its
 * original defaults and requested update have been completely constructed. */
static uint32_t ku_task_host_http_header_set(KuTaskAdapterHostV1* host,
    KuStruct___ku_http_response* response,KuString key,KuString value) {
  if(!response || !ku_task_http_header_string(key) || !ku_task_http_header_string(value))
    return KU_TASK_DRIVER_INVALID_ARGUMENT;
  KuObject* map=response->headers;
  KuValue* prior=map?ku_object_get(map,key):NULL;
  if(prior && prior->tag!=KU_STR)return KU_TASK_DRIVER_INVALID_ARGUMENT;
  size_t bytes=ku_task_http_header_copy_bytes(value);
  if(!prior && !ku_task_http_header_add(&bytes,ku_task_http_header_copy_bytes(key)))return KU_TASK_DRIVER_LIMIT;
  if(map) {
    if(!prior && map->len+1u>=map->cap-map->cap/4u) {
      /* Keep generated states inside the same bound accepted by the raw
       * adapter. Reject before reserving or mutating; existing-key replacement
       * needs no growth and remains valid at the maximum capacity. */
      if(map->cap>=65536u)return KU_TASK_DRIVER_LIMIT;
      if(map->cap>SIZE_MAX/(2u*sizeof(KuEntry)) || !ku_task_http_header_add(&bytes,map->cap*2u*sizeof(KuEntry)))return KU_TASK_DRIVER_LIMIT;
    }
  } else {
    if(!ku_task_http_header_add(&bytes,sizeof(KuObject)+8u*sizeof(KuEntry)))return KU_TASK_DRIVER_LIMIT;
    if(response->content_type.len && !ku_task_http_header_add(&bytes,ku_task_http_header_copy_bytes(response->content_type)))return KU_TASK_DRIVER_LIMIT;
    if(response->location.len && !ku_task_http_header_add(&bytes,ku_task_http_header_copy_bytes(response->location)))return KU_TASK_DRIVER_LIMIT;
  }
  uint32_t status=ku_task_http_header_reserve(host,bytes);
  if(status!=KU_TASK_DRIVER_OK)return status;
  KuString copied_key={0},copied_value={0};KuObject* created=NULL;
  if(!ku_object_string_try_clone(value,&copied_value))goto failed;
  if(!prior && !ku_object_string_try_clone(key,&copied_key))goto failed;
  if(!map) {
    if(!ku_object_try_new(8u,&created))goto failed;
    const char* names[2]={"content-type","location"};
    KuString defaults[2]={response->content_type,response->location};
    for(size_t i=0;i<2u;i++)if(defaults[i].len) {
      KuString copied={0};if(!ku_object_string_try_clone(defaults[i],&copied))goto failed;
      KuValue field=ku_v_str(copied);
      if(!ku_object_try_set_copy_key(created,ku_string_static((const uint8_t*)names[i],strlen(names[i])),&field)) {
        ku_value_drop(&field);goto failed;
      }
    }
    map=created;
  }
  if(prior) {
    ku_value_drop(prior);*prior=ku_v_str(ku_string_move(&copied_value));
  } else {
    KuValue field=ku_v_str(ku_string_move(&copied_value));
    if(!ku_object_try_set_owned(map,&copied_key,&field)) {
      ku_value_drop(&field);goto failed;
    }
  }
  if(created)response->headers=created;
  /* The next frame reconciliation observes both the new map and old replaced
   * bytes already freed. Until then the reserved peak stays conservatively held. */
  return KU_TASK_DRIVER_OK;
failed:
  ku_string_drop(&copied_key);ku_string_drop(&copied_value);ku_object_drop(created);
  return ku_task_http_header_failed(host);
}
