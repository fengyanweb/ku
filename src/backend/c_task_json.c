/* Frame-local JSON owners use the existing JSON/object engine. This ledger
 * covers Ku-requested heap bytes, not allocator metadata, libc internals,
 * thread stacks or RSS. No owner escapes through the Task outcome ABI.
 * Every allocating operation prepays its transient peak before calling the
 * engine. The generated caller publishes the result and clears consumed slots
 * before reconciliation can refund anything. */
#define KU_TASK_JSON_OWNER_DEPTH 64u
static uint32_t ku_task_json_add(size_t* total,size_t bytes) {
  if(bytes>SIZE_MAX-*total)return KU_TASK_DRIVER_LIMIT;
  *total+=bytes;return KU_TASK_DRIVER_OK;
}
static uint32_t ku_task_json_string_charge(KuString value,size_t* total) {
  if(value.storage==KU_STRING_STATIC)
    return value.capacity || (value.len && !value.ptr)?KU_TASK_DRIVER_INVALID_ARGUMENT:KU_TASK_DRIVER_OK;
  if(value.storage!=KU_STRING_OWNED || value.len>value.capacity
      || (!value.ptr && (value.len || value.capacity)))return KU_TASK_DRIVER_INVALID_ARGUMENT;
  return ku_task_json_add(total,value.ptr?(value.capacity?value.capacity:1u):0u);
}
/* Source construction and move-only frame storage exclude cycles and shared
 * owning subtrees. The explicit depth guard also bounds this scanner's stack;
 * the JSON parser/writer retain their separate, original depth-32 rule. */
static uint32_t ku_task_json_owned_charge(KuValue value,size_t* total,size_t depth) {
  if(depth>KU_TASK_JSON_OWNER_DEPTH)return KU_TASK_DRIVER_LIMIT;
  uint32_t status=KU_TASK_DRIVER_OK;
  switch(value.tag) {
    case KU_NULL:case KU_INT:case KU_FLOAT:case KU_BOOL:return status;
    case KU_STR:return ku_task_json_string_charge(value.as.s,total);
    case KU_OBJECT: {
      KuObject* object=value.as.o;
      if(!object || !object->entries || object->len>object->cap
          || object->cap>SIZE_MAX/sizeof(KuEntry))return KU_TASK_DRIVER_INVALID_ARGUMENT;
      status=ku_task_json_add(total,sizeof(*object));
      if(!status)status=ku_task_json_add(total,object->cap*sizeof(KuEntry));
      size_t used=0;
      for(size_t i=0;!status && i<object->cap;i++)if(object->entries[i].used) {
        used++;
        status=ku_task_json_string_charge(object->entries[i].key,total);
        if(!status)status=ku_task_json_owned_charge(object->entries[i].value,total,depth+1u);
      }
      return status?status:used==object->len?KU_TASK_DRIVER_OK:KU_TASK_DRIVER_INVALID_ARGUMENT;
    }
    case KU_ARRAY: {
      KuValueArray* array=value.as.arr;
      if(!array || array->len>array->cap || (array->cap && !array->data)
          || array->cap>SIZE_MAX/sizeof(KuValue))return KU_TASK_DRIVER_INVALID_ARGUMENT;
      status=ku_task_json_add(total,sizeof(*array));
      if(!status)status=ku_task_json_add(total,array->cap*sizeof(KuValue));
      for(size_t i=0;!status && i<array->len;i++)
        status=ku_task_json_owned_charge(array->data[i],total,depth+1u);
      return status;
    }
    default:return KU_TASK_DRIVER_INVALID_ARGUMENT;
  }
}
static size_t ku_task_json_buffer_capacity(size_t length) {
  if(!length)return 0u;
  size_t capacity=64u;
  while(capacity<length && capacity<KU_JSON_MAX_OUTPUT_BYTES)capacity*=2u;
  return capacity>KU_JSON_MAX_OUTPUT_BYTES?KU_JSON_MAX_OUTPUT_BYTES:capacity;
}
static uint32_t ku_task_json_product_add(size_t* total,size_t count,size_t width) {
  if(width && count>SIZE_MAX/width)return KU_TASK_DRIVER_LIMIT;
  return ku_task_json_add(total,count*width);
}
/* Lexical counts deliberately overestimate malformed prefixes and duplicate
 * keys. They do not reinterpret numbers or change the parser's errors. Twice
 * table/string capacity covers old/new storage coexisting during growth. */
static uint32_t ku_task_json_parse_reservation(KuString text,size_t* total) {
  size_t objects=0,arrays=0,strings=0,values=0,string_bytes=0,numeric=0;
  for(size_t i=0;i<text.len;) {
    uint8_t byte=text.ptr[i++];
    if(byte=='{'){objects++;values++;continue;}
    if(byte=='['){arrays++;values++;continue;}
    if(byte=='"') {
      strings++;values++;size_t begin=i;bool escaped=false;
      while(i<text.len) {
        uint8_t c=text.ptr[i++];
        if(!escaped && c=='"'){i--;break;}
        if(!escaped && c=='\\')escaped=true;else escaped=false;
      }
      size_t raw=i-begin;
      if(ku_task_json_add(&string_bytes,ku_task_json_buffer_capacity(raw)))return KU_TASK_DRIVER_LIMIT;
      if(i<text.len && text.ptr[i]=='"')i++;
      continue;
    }
    if(byte=='}' || byte==']' || byte==':' || byte==',' || byte==' ' || byte=='\t' || byte=='\r' || byte=='\n')continue;
    size_t begin=i-1u;values++;
    while(i<text.len) {
      uint8_t c=text.ptr[i];
      if(c=='{' || c=='[' || c=='"' || c=='}' || c==']' || c==':' || c==',' || c==' ' || c=='\t' || c=='\r' || c=='\n')break;
      i++;
    }
    if((byte=='-' || (byte>='0' && byte<='9')) && i-begin>numeric)numeric=i-begin;
  }
  uint32_t status=ku_task_json_product_add(total,objects,sizeof(KuObject));
  if(!status)status=ku_task_json_product_add(total,arrays,sizeof(KuValueArray));
  if(!status)status=ku_task_json_product_add(total,objects,16u*sizeof(KuEntry));
  if(!status)status=ku_task_json_product_add(total,strings,8u*sizeof(KuEntry));
  if(!status)status=ku_task_json_product_add(total,arrays,16u*sizeof(KuValue));
  if(!status)status=ku_task_json_product_add(total,values,4u*sizeof(KuValue));
  if(!status)status=ku_task_json_product_add(total,string_bytes,2u);
  if(!status)status=ku_task_json_add(total,numeric+1u+66u);
  return status;
}
static uint32_t ku_task_json_reserve(KuTaskAdapterHostV1* host,size_t bytes) {
  if(!host || !host->ticket || !host->control
      || host->allocation_host_bytes!=sizeof(*host)
      || host->allocation_host_abi!=KU_TASK_ALLOCATION_HOST_ABI_VERSION
      || !host->allocation_context || !host->reconcile_charge)return KU_TASK_DRIVER_INVALID_ARGUMENT;
  uint32_t status=host->reconcile_charge(host->allocation_context);
  if(!status)status=ku_task_driver_grow_charge(host->ticket,bytes);
  return status;
}
static KuResult_kuvalue ku_task_json_limit(void) {
  return (KuResult_kuvalue){false,{0},ku_json_error("resource_limit","Task JSON byte or depth budget exhausted")};
}
static KuResult_kuvalue ku_task_host_json_decode(KuTaskAdapterHostV1* host,KuString text) {
  size_t bytes=0;
  if(text.len>KU_JSON_MAX_INPUT_BYTES || (text.len && !text.ptr))return ku_json_parse(text);
  if(ku_task_json_parse_reservation(text,&bytes) || ku_task_json_reserve(host,bytes))return ku_task_json_limit();
  return ku_json_parse(text);
}
static uint32_t ku_task_json_encode_bound(KuValue value,size_t depth,size_t* output,size_t* scratch) {
  if(depth>KU_TASK_JSON_OWNER_DEPTH)return KU_TASK_DRIVER_LIMIT;
  switch(value.tag) {
    case KU_NULL:ku_task_json_capacity_add(output,4u);return KU_TASK_DRIVER_OK;
    case KU_BOOL:ku_task_json_capacity_add(output,5u);return KU_TASK_DRIVER_OK;
    case KU_INT:ku_task_json_capacity_add(output,20u);return KU_TASK_DRIVER_OK;
    case KU_FLOAT:ku_task_json_capacity_add(output,KU_DTOA_TEXT_CAP+2u);return KU_TASK_DRIVER_OK;
    case KU_STR:return ku_task_json_string_capacity(value.as.s,output)?KU_TASK_DRIVER_OK:KU_TASK_DRIVER_INVALID_ARGUMENT;
    case KU_ARRAY: {
      KuValueArray* array=value.as.arr;if(!array)return KU_TASK_DRIVER_INVALID_ARGUMENT;
      ku_task_json_capacity_add(output,2u);ku_task_json_capacity_add(output,array->len);
      for(size_t i=0;i<array->len;i++) {
        uint32_t status=ku_task_json_encode_bound(array->data[i],depth+1u,output,scratch);
        if(status)return status;
      }
      return KU_TASK_DRIVER_OK;
    }
    case KU_OBJECT: {
      KuObject* object=value.as.o;if(!object)return KU_TASK_DRIVER_INVALID_ARGUMENT;
      if(object->len>8u && ku_task_json_product_add(scratch,object->len,sizeof(KuEntry*)))return KU_TASK_DRIVER_LIMIT;
      ku_task_json_capacity_add(output,2u);
      for(size_t i=0;i<object->cap;i++)if(object->entries[i].used) {
        ku_task_json_capacity_add(output,2u);
        if(!ku_task_json_string_capacity(object->entries[i].key,output))return KU_TASK_DRIVER_INVALID_ARGUMENT;
        uint32_t status=ku_task_json_encode_bound(object->entries[i].value,depth+1u,output,scratch);
        if(status)return status;
      }
      return KU_TASK_DRIVER_OK;
    }
    default:return KU_TASK_DRIVER_INVALID_ARGUMENT;
  }
}
static KuResult_str ku_task_host_json_encode(KuTaskAdapterHostV1* host,KuValue value) {
  size_t live=0,output=0,scratch=0;
  uint32_t status=ku_task_json_owned_charge(value,&live,0u);
  if(!status)status=ku_task_json_encode_bound(value,0u,&output,&scratch);
  if(!status)status=ku_task_json_product_add(&scratch,ku_task_json_buffer_capacity(output),2u);
  if(!status)status=ku_task_json_reserve(host,scratch);
  if(status)return (KuResult_str){false,{0},ku_json_error("resource_limit","Task JSON byte or depth budget exhausted")};
  return ku_json_stringify(value);
}
static uint32_t ku_task_json_lookup_reservation(KuValue value,KuString key,size_t* bytes) {
  if(value.tag!=KU_OBJECT)return KU_TASK_DRIVER_OK;
  KuValue* found=ku_object_get(value.as.o,key);
  return found?ku_task_json_owned_charge(*found,bytes,0u):ku_task_json_add(bytes,key.len>SIZE_MAX-20u?SIZE_MAX:key.len+20u);
}
static KuResult_kuvalue ku_task_host_json_get(KuTaskAdapterHostV1* host,KuValue value,KuString key) {
  size_t bytes=0;
  if(ku_task_json_lookup_reservation(value,key,&bytes) || ku_task_json_reserve(host,bytes))return ku_task_json_limit();
  return ku_value_get_result(value,key);
}
static KuResult_kuvalue ku_task_host_json_take(KuTaskAdapterHostV1* host,KuValue* source,KuString key) {
  size_t bytes=0;
  if(source->tag==KU_OBJECT && !ku_object_get(source->as.o,key)
      && ku_task_json_add(&bytes,key.len>SIZE_MAX-20u?SIZE_MAX:key.len+20u)) {
    ku_value_drop(source);return ku_task_json_limit();
  }
  if(ku_task_json_reserve(host,bytes)){ku_value_drop(source);return ku_task_json_limit();}
  return ku_value_take_result(ku_value_move(source),key);
}
static KuResult_int ku_task_host_json_as_int(KuTaskAdapterHostV1* host,KuValue* source) {
  (void)host;return ku_value_as_int(ku_value_move(source));
}
static KuResult_str ku_task_host_json_as_str(KuTaskAdapterHostV1* host,KuValue* source) {
  (void)host;return ku_value_as_str(ku_value_move(source));
}
/* All input payloads are consumed even on failure. Until this call returns,
 * their frame slots remain registered; do not reconcile after dropping or
 * moving them. The generated caller clears those slots before publishing the
 * returned owner/error and reaching its next reconciliation point. */
static KuResult_kuvalue ku_task_host_json_build_object(KuTaskAdapterHostV1* host,
    const KuString* keys,KuValue* values,size_t count) {
  size_t capacity=0,bytes=0;uint32_t status=KU_TASK_DRIVER_OK;
  if(count>64u || (count && (!keys || !values)) || !ku_object_try_capacity(count*2u,&capacity))status=KU_TASK_DRIVER_INVALID_ARGUMENT;
  if(!status)status=ku_task_json_add(&bytes,sizeof(KuObject));
  if(!status)status=ku_task_json_product_add(&bytes,capacity,sizeof(KuEntry));
  for(size_t i=0;!status && i<count;i++) {
    size_t live=0;
    status=ku_task_json_owned_charge(values[i],&live,1u);
    if(!status)status=ku_task_json_add(&bytes,keys[i].len?keys[i].len:1u);
  }
  if(!status)status=ku_task_json_reserve(host,bytes);
  KuResult_kuvalue result=ku_task_json_limit();KuObject* object=NULL;
  if(!status) {
    if(!ku_object_try_new(capacity,&object))result=ku_object_out_of_memory_result();
    else {
      bool ok=true;
      for(size_t i=0;i<count;i++)if(!ku_object_try_set_copy_key(object,keys[i],&values[i])){ok=false;break;}
      if(ok){result=(KuResult_kuvalue){true,ku_v_object(object),{0}};object=NULL;}
      else result=ku_object_out_of_memory_result();
    }
  }
  if(object)ku_object_drop(object);
  if(values)for(size_t i=0;i<count;i++)ku_value_drop(&values[i]);
  return result;
}
