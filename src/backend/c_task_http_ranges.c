/* Private, bounded HTTP aggregate validation. Only string-valued maps are
 * accepted. Caller-owned raw pointers retain the adapter's readable-extent
 * precondition; known headers/runtime/owned allocations may never alias. */
static uint32_t ku_task_adapter_http_range_excludes(KuTaskAdapterBufferV1 value,
    const KuTaskAdapterBufferV1* base,size_t base_count,
    KuObject* const* objects,size_t object_count,size_t entry_count,
    const KuTaskAdapterBufferV1* headers,size_t header_count,
    const KuTaskAdapterBufferV1* extra,size_t extra_count,KuTaskDriverV1* driver) {
  uint32_t status=ku_task_adapter_buffer_excludes(value,headers,header_count,extra,extra_count,driver);
  if(status!=KU_TASK_DRIVER_OK)return status;
  for(size_t i=0;i<base_count;i++)if(ku_task_adapter_buffers_overlap(value,base[i]))return KU_TASK_DRIVER_INVALID_ARGUMENT;
  for(size_t i=0;i<object_count;i++)if(objects[i]) {
    KuTaskAdapterBufferV1 header={objects[i],sizeof(KuObject),1u};
    if(ku_task_adapter_buffers_overlap(value,header))return KU_TASK_DRIVER_INVALID_ARGUMENT;
  }
  for(size_t i=0;i<entry_count;i++)if(objects[i]) {
    KuTaskAdapterBufferV1 entries={objects[i]->entries,objects[i]->cap*sizeof(KuEntry),1u};
    if(ku_task_adapter_buffers_overlap(value,entries))return KU_TASK_DRIVER_INVALID_ARGUMENT;
  }
  return KU_TASK_DRIVER_OK;
}
static uint32_t ku_task_adapter_http_ranges(KuTaskDriverV1* driver,
    KuObject* const* objects,size_t object_count,
    const KuTaskAdapterBufferV1* base,size_t base_count,
    const KuTaskAdapterStringVectorV1* vectors,size_t vector_count,
    const KuTaskAdapterBufferV1* headers,size_t header_count,
    const KuTaskAdapterBufferV1* extra,size_t extra_count,int read_leaves,size_t* total) {
  if(object_count>192u)return KU_TASK_DRIVER_LIMIT;
  uint32_t status=KU_TASK_DRIVER_OK;
  /* Register all map headers, then all complete entry arrays. No entry/key
   * metadata is read before exclusion of the surrounding typed allocations. */
  for(size_t i=0;i<object_count;i++)if(objects[i]) {
    KuObject* map=objects[i];
    if(!ku_task_frame_storage_valid(map,sizeof(*map),sizeof(*map),KU_TASK_FRAME_ALIGNOF(KuObject)))return KU_TASK_DRIVER_INVALID_ARGUMENT;
    KuTaskAdapterBufferV1 range={map,sizeof(*map),1u};
    status=ku_task_adapter_http_range_excludes(range,base,base_count,objects,i,0,headers,header_count,extra,extra_count,driver);
    if(status!=KU_TASK_DRIVER_OK)return status;
  }
  for(size_t i=0;i<object_count;i++)if(objects[i]) {
    KuObject* map=objects[i];
    if(!map->entries || map->cap<8u || (map->cap&(map->cap-1u)) || map->cap>65536u || map->len>=map->cap-map->cap/4u || map->cap>(SIZE_MAX-sizeof(*map))/sizeof(KuEntry))return KU_TASK_DRIVER_INVALID_ARGUMENT;
  }
  for(size_t i=0;i<object_count;i++)if(objects[i]) {
    KuObject* map=objects[i];
    size_t bytes=map->cap*sizeof(KuEntry);
    if(!ku_task_frame_storage_valid(map->entries,bytes,bytes,KU_TASK_FRAME_ALIGNOF(KuEntry)))return KU_TASK_DRIVER_INVALID_ARGUMENT;
    KuTaskAdapterBufferV1 range={map->entries,bytes,1u};
    status=ku_task_adapter_http_range_excludes(range,base,base_count,objects,object_count,i,headers,header_count,extra,extra_count,driver);
    if(status!=KU_TASK_DRIVER_OK)return status;
    size_t charge=sizeof(*map)+bytes;
    if(charge>SIZE_MAX-*total)return KU_TASK_DRIVER_LIMIT;*total+=charge;
  }
  if(!read_leaves)return KU_TASK_DRIVER_OK;
  size_t count=0;
  for(size_t i=0;i<vector_count;i++){if(vectors[i].len>640000u-count)return KU_TASK_DRIVER_LIMIT;count+=vectors[i].len;}
  for(size_t i=0;i<object_count;i++)if(objects[i]){if(objects[i]->len>(640000u-count)/2u)return KU_TASK_DRIVER_LIMIT;count+=objects[i]->len*2u;}
  KuTaskAdapterBufferV1 local[64],*leaves=local;size_t scratch_bytes=0,at=0;
  if(driver && count>64u){status=ku_task_adapter_scratch(driver,count,&leaves,&scratch_bytes);if(status!=KU_TASK_DRIVER_OK)return status;}
  for(size_t v=0;v<vector_count && status==KU_TASK_DRIVER_OK;v++)for(size_t i=0;i<vectors[v].len;i++) {
    KuString leaf=vectors[v].data[i];status=ku_task_adapter_string_charge(leaf,total);if(status!=KU_TASK_DRIVER_OK)break;
    KuTaskAdapterBufferV1 range=ku_task_adapter_string_buffer(leaf);
    status=ku_task_adapter_http_range_excludes(range,base,base_count,objects,object_count,object_count,headers,header_count,extra,extra_count,driver);
    if(status!=KU_TASK_DRIVER_OK)break;if(driver)leaves[at++]=range;
  }
  for(size_t m=0;m<object_count && status==KU_TASK_DRIVER_OK;m++)if(objects[m]) {
    KuObject* map=objects[m];size_t used=0;
    for(size_t i=0;i<map->cap && status==KU_TASK_DRIVER_OK;i++)if(map->entries[i].used) {
      if(++used>map->len || map->entries[i].value.tag!=KU_STR){status=KU_TASK_DRIVER_INVALID_ARGUMENT;break;}
      KuString strings[2]={map->entries[i].key,map->entries[i].value.as.s};
      for(size_t k=0;k<2;k++) {
        status=ku_task_adapter_string_charge(strings[k],total);if(status!=KU_TASK_DRIVER_OK)break;
        KuTaskAdapterBufferV1 range=ku_task_adapter_string_buffer(strings[k]);
        status=ku_task_adapter_http_range_excludes(range,base,base_count,objects,object_count,object_count,headers,header_count,extra,extra_count,driver);
        if(status!=KU_TASK_DRIVER_OK)break;if(driver)leaves[at++]=range;
      }
    }
    if(status==KU_TASK_DRIVER_OK && used!=map->len)status=KU_TASK_DRIVER_INVALID_ARGUMENT;
  }
  if(driver && status==KU_TASK_DRIVER_OK)status=at==count?ku_task_adapter_ranges_disjoint(leaves,count):KU_TASK_DRIVER_INVALID_ARGUMENT;
  return scratch_bytes?ku_task_adapter_scratch_free(driver,leaves,scratch_bytes,status):status;
}
