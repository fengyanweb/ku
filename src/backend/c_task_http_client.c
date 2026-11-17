/* One HTTP operation uses one existing generation-checked network job. The
 * only blocking phase is DNS on the fixed resolver workers. This code never
 * performs poll/select or sleeps; every socket remains nonblocking. */
static uint32_t ku_task_http_reserve(void* raw,size_t bytes) {
  KuTaskBlockingJobV1* job=(KuTaskBlockingJobV1*)raw;
  KuTaskBlockingOutputV1 allocation={job->http_pool,job,{NULL,NULL},{0,0},KU_TASK_DRIVER_OK,NULL,0,0,0};
  return ku_task_blocking_output_reserve(&allocation,bytes);
}
static intptr_t ku_task_http_read(void* raw,uint8_t* bytes,size_t capacity) {
  KuTaskBlockingJobV1* job=(KuTaskBlockingJobV1*)raw;
  if(!job->net_client || capacity>65536u || ku_net_now_ms()>=job->net_deadline || ku_task_control_atomic_load(&job->http_control->phase)!=KU_TASK_CONTROL_LIVE)return -2;
  int n=ku_net_socket_recv(job->net_client->socket_value,bytes,capacity);
  if(n>=0)return n;int failure=ku_net_socket_last_error();
  return ku_net_socket_error_would_block(failure)||ku_net_socket_error_interrupted(failure)?-1:-2;
}
static intptr_t ku_task_http_write(void* raw,const uint8_t* bytes,size_t length) {
  KuTaskBlockingJobV1* job=(KuTaskBlockingJobV1*)raw;
  if(!job->net_client || length>65536u || ku_net_now_ms()>=job->net_deadline || ku_task_control_atomic_load(&job->http_control->phase)!=KU_TASK_CONTROL_LIVE)return -2;
  int n=ku_net_socket_send(job->net_client->socket_value,bytes,length);
  if(n>=0)return n;int failure=ku_net_socket_last_error();
  return ku_net_socket_error_would_block(failure)||ku_net_socket_error_interrupted(failure)?-1:-2;
}
static void ku_task_http_note_abi_panic(KuTaskDriverV1* driver) {
  if(driver&&!ku_task_driver_lock(driver)){driver->fault=KU_TASK_DRIVER_INTERNAL;ku_task_driver_unlock(driver);}
}
static uint32_t ku_task_http_refund(void* raw,size_t bytes) {
  KuTaskBlockingJobV1* job=(KuTaskBlockingJobV1*)raw;
  KuTaskBlockingOutputV1 allocation={job->http_pool,job,{NULL,NULL},{0,0},KU_TASK_DRIVER_OK,NULL,0,0,0};
  ku_task_blocking_output_refund(&allocation,bytes);
  if(allocation.status!=KU_TASK_DRIVER_OK)ku_task_http_note_abi_panic(job->ticket.driver);
  return allocation.status;
}
static void ku_task_http_drop_operation(KuTaskBlockingJobV1* job) {
  if(job->http_operation){KuHttpClientAsyncV1* operation=job->http_operation;job->http_operation=NULL;if(__ku_http_client_async_drop_v1(operation)!=0)ku_task_http_note_abi_panic(job->ticket.driver);}
  ku_drop_net_client(&job->net_client);
}
static void ku_task_http_cache_drop_locked(KuTaskHttpIdleV1* idle) {
  if(!idle->operation)return;
  KuHttpClientAsyncV1* operation=idle->operation;idle->operation=NULL;
  if(__ku_http_client_async_drop_v1(operation)!=0)ku_task_http_note_abi_panic(idle->driver);
  ku_drop_net_client(&idle->client);
  if(ku_task_driver_scratch_refund(idle->driver,idle->bytes)!=KU_TASK_DRIVER_OK){if(!ku_task_driver_lock(idle->driver)){idle->driver->fault=KU_TASK_DRIVER_INTERNAL;ku_task_driver_unlock(idle->driver);}}
  *idle=(KuTaskHttpIdleV1){0};
}
static void ku_task_http_cache_close_locked(KuTaskBlockingPoolV1* pool) {
  for(size_t i=0;i<KU_TASK_HTTP_IDLE_CAPACITY;i++)ku_task_http_cache_drop_locked(&pool->http_idle[i]);
}
/* Match the synchronous engine's reuse admission: only a nonblocking
 * WouldBlock peek is an idle transport. EOF, unsolicited bytes or any other
 * error evicts before this request sends anything; no request is retried. */
static int ku_task_http_cache_usable(KuNetClient* client) {
  if(!client||client->socket_value==KU_NET_INVALID_SOCKET)return 0;
  uint8_t byte;
#if defined(_WIN32)
  int n=recv(client->socket_value,(char*)&byte,1,MSG_PEEK);
#else
  int n=(int)recv(client->socket_value,&byte,1,MSG_PEEK);
#endif
  if(n>=0)return 0;
  return ku_net_socket_error_would_block(ku_net_socket_last_error());
}
static int ku_task_http_cache_take(KuTaskBlockingJobV1* job,const KuHttpClientAsyncInfoV1* info) {
  KuTaskBlockingPoolV1* pool=job->http_pool;KuTaskHttpIdleV1 idle={0};
  if(ku_task_blocking_lock(pool))return 0;
  if(!pool->closing)for(size_t i=0;i<KU_TASK_HTTP_IDLE_CAPACITY;i++){
    KuTaskHttpIdleV1* entry=&pool->http_idle[i];if(!entry->operation||entry->driver!=job->ticket.driver)continue;
    KuHttpClientAsyncInfoV1 old={0};
    if(__ku_http_client_async_info_v1(entry->operation,&old)==0&&old.port==info->port&&old.tls==info->tls&&old.host.len==info->host.len&&(!old.host.len||!memcmp(old.host.data,info->host.data,old.host.len))){idle=*entry;*entry=(KuTaskHttpIdleV1){0};break;}
  }
  ku_task_blocking_unlock(pool);if(!idle.operation)return 0;
  int adopted=0;uint32_t reused=2u;
  if(ku_task_http_cache_usable(idle.client)&&ku_task_http_reserve(job,idle.bytes)==KU_TASK_DRIVER_OK){
    reused=__ku_http_client_async_reuse_v1(job->http_operation,idle.operation);
    /* 2 is the sole non-consuming return. Both success and a caught drop
     * panic consumed the old operation, so it must never be dropped twice. */
    if(reused!=2u)idle.operation=NULL;
    if(reused==0u){job->net_client=idle.client;idle.client=NULL;job->net_resolved=1;job->http_connected=1;job->http_interest=KU_TASK_NET_WRITE;adopted=1;}
    else if(reused!=2u){job->http_result=(KuResult_struct___ku_http_response){false,{0},__ku_http_client_error("internal_panic","HTTP cached transport failed internally")};job->http_finished=1;job->net_resolved=1;ku_task_http_note_abi_panic(job->ticket.driver);adopted=1;}
  }
  if(idle.operation&&__ku_http_client_async_drop_v1(idle.operation)!=0)ku_task_http_note_abi_panic(idle.driver);
  ku_drop_net_client(&idle.client);if(ku_task_driver_scratch_refund(idle.driver,idle.bytes)!=KU_TASK_DRIVER_OK){if(!ku_task_driver_lock(idle.driver)){idle.driver->fault=KU_TASK_DRIVER_INTERNAL;ku_task_driver_unlock(idle.driver);}}return adopted;
}
static void ku_task_http_cache_store(KuTaskBlockingJobV1* job) {
  size_t bytes=__ku_http_client_async_park_v1(job->http_operation);
  if(!bytes||!job->net_client||bytes>SIZE_MAX-sizeof(KuNetClient))return;
  bytes+=sizeof(KuNetClient);KuTaskBlockingPoolV1* pool=job->http_pool;
  if(ku_task_blocking_lock(pool))return;
  if(!pool->closing&&ku_task_control_atomic_load(&job->http_control->phase)==KU_TASK_CONTROL_LIVE&&ku_task_driver_scratch_reserve(job->ticket.driver,bytes)==KU_TASK_DRIVER_OK){
    size_t at=KU_TASK_HTTP_IDLE_CAPACITY;KuHttpClientAsyncInfoV1 current={0};
    (void)__ku_http_client_async_info_v1(job->http_operation,&current);
    /* Match the shared engine's default: at most one idle connection per
     * origin and 100 in total. An idle transport has no Task owner or active
     * registration; a fresh job/generation acquires its sole physical owner. */
    for(size_t i=0;i<KU_TASK_HTTP_IDLE_CAPACITY;i++){
      KuTaskHttpIdleV1* entry=&pool->http_idle[i];if(!entry->operation)continue;
      KuHttpClientAsyncInfoV1 old={0};
      if(__ku_http_client_async_info_v1(entry->operation,&old)==0&&old.port==current.port&&old.tls==current.tls&&old.host.len==current.host.len&&(!old.host.len||!memcmp(old.host.data,current.host.data,old.host.len))){at=i;ku_task_http_cache_drop_locked(entry);break;}
    }
    if(at==KU_TASK_HTTP_IDLE_CAPACITY)for(size_t i=0;i<KU_TASK_HTTP_IDLE_CAPACITY;i++)if(!pool->http_idle[i].operation){at=i;break;}
    if(at==KU_TASK_HTTP_IDLE_CAPACITY){at=pool->http_idle_cursor++%KU_TASK_HTTP_IDLE_CAPACITY;ku_task_http_cache_drop_locked(&pool->http_idle[at]);}
    pool->http_idle[at]=(KuTaskHttpIdleV1){job->http_operation,job->net_client,job->ticket.driver,job->net_owner,bytes};job->http_operation=NULL;job->net_client=NULL;
  }
  ku_task_blocking_unlock(pool);
}
static void ku_task_http_resolve(KuTaskBlockingJobV1* job,int cancelled,KuTaskBlockingOutputV1* allocation) {
  if(cancelled){job->http_result=(KuResult_struct___ku_http_response){false,{0},ku_task_blocking_error("cancelled","HTTP request cancelled before DNS")};job->http_finished=1;return;}
  KuHttpClientAsyncInfoV1 info={0};
  if(__ku_http_client_async_info_v1(job->http_operation,&info)!=0){job->http_result=(KuResult_struct___ku_http_response){false,{0},__ku_http_client_error("transport","HTTP operation identity failed")};job->http_finished=1;return;}
  if(info.done){job->net_resolved=1;return;}
  if(ku_task_http_cache_take(job,&info))return;
  /* This temporary map borrows immutable operation-owned host storage. The
   * resolver copies OS addresses before returning; no addrinfo escapes it. */
  KuEntry entries[8];memset(entries,0,sizeof(entries));KuObject config={0,8,entries};
  KuString keys[2]={ku_string_static((const uint8_t*)"host",4),ku_string_static((const uint8_t*)"port",4)};
  KuValue values[2]={ku_v_str(ku_string_static(info.host.data,info.host.len)),ku_v_int((int64_t)info.port)};
  for(size_t i=0;i<2;i++){size_t at=(size_t)ku_obj_hash(keys[i])&7u;while(entries[at].used)at=(at+1u)&7u;entries[at]=(KuEntry){keys[i],values[i],true};config.len++;}
  ku_task_net_resolve(job,&config,allocation);
  if(!job->net_resolved){
    KuError failure=ku_error_move(&job->net_connect_result.error);
    KuError http=__ku_http_client_error(allocation->status==KU_TASK_DRIVER_LIMIT?"memory_limit":allocation->status==KU_TASK_BLOCKING_OUT_OF_MEMORY?"out_of_memory":"transport","HTTP host resolution failed");
    if(failure.message.len){http.message=failure.message;failure.message=(KuString){0};}ku_error_drop(&failure);
    job->http_result=(KuResult_struct___ku_http_response){false,{0},http};job->http_finished=1;
  }
}
static int ku_task_http_finish(KuTaskBlockingPoolV1* pool,KuTaskBlockingJobV1* job,KuError error) {
  if(error.code.len){ku_result_drop_struct___ku_http_response(&job->http_result);job->http_result=(KuResult_struct___ku_http_response){false,{0},error};}
  else if(!job->http_finished){
    __ku_http_client_output_v1 output={0};output.size=sizeof(output);
    uint32_t status=__ku_http_client_async_take_v1(job->http_operation,&output);
    size_t capacity=0,copy=0;int valid=1;
    if(status==0){
      valid=ku_object_try_capacity(output.header_count,&capacity);
      if(valid)copy=sizeof(KuObject)+capacity*sizeof(KuEntry);
      size_t n=output.body.len?output.body.len:1u;if(n>SIZE_MAX-copy)valid=0;else copy+=n;
      for(size_t i=0;valid&&i<output.header_count;i++){
        size_t n=output.headers[i].name.len?output.headers[i].name.len:1u,v=output.headers[i].value.len?output.headers[i].value.len:1u;
        if(n>SIZE_MAX-copy||v>SIZE_MAX-copy-n)valid=0;else copy+=n+v;
      }
      /* Current insertion grows at the existing three-quarter threshold: prepay the simultaneous old and
       * new table of every bounded rehash, not only the final table. */
      size_t used=0,cap=capacity;
      while(valid&&used<output.header_count){
        if(used+1u>=cap-cap/4u){if(cap>SIZE_MAX/2u||cap*2u>SIZE_MAX/sizeof(KuEntry)){valid=0;break;}cap*=2u;size_t grow=cap*sizeof(KuEntry);if(grow>SIZE_MAX-copy){valid=0;break;}copy+=grow;}used++;
      }
    }else{
      size_t n=output.code.len,v=output.message.len;if(n>SIZE_MAX-v)valid=0;else copy=n+v;
    }
    if(!valid||ku_task_http_reserve(job,copy)!=KU_TASK_DRIVER_OK)job->http_result=(KuResult_struct___ku_http_response){false,{0},__ku_http_client_error("memory_limit","HTTP response exceeds Task byte budget")};
    else job->http_result=__ku_http_client_result(&output,status);
    __ku_http_client_release_v1(&output);
  }
  (void)pool;job->http_finished=1;if(job->http_result.ok)ku_task_http_cache_store(job);ku_task_http_drop_operation(job);return 1;
}
/* Returns 2 only to re-enter the existing DNS queue after a real redirect.
 * A redirect retains the same absolute deadline, job and Task registration. */
static int ku_task_http_step(KuTaskBlockingPoolV1* pool,KuTaskBlockingJobV1* job,uint32_t events,int cancelled) {
  if(cancelled)return ku_task_http_finish(pool,job,ku_task_blocking_error("cancelled","HTTP request was cancelled"));
  if(!job->net_deadline||ku_net_now_ms()>=job->net_deadline)return ku_task_http_finish(pool,job,__ku_http_client_error("transport","HTTP request deadline expired"));
  if(job->http_finished)return ku_task_http_finish(pool,job,(KuError){0});
  KuHttpClientAsyncInfoV1 info={0};if(__ku_http_client_async_info_v1(job->http_operation,&info)!=0)return ku_task_http_finish(pool,job,__ku_http_client_error("transport","invalid HTTP operation"));
  if(info.done)return ku_task_http_finish(pool,job,(KuError){0});
  KuNetClient* client=job->net_client;if(!client)return ku_task_http_finish(pool,job,__ku_http_client_error("transport","HTTP resolver did not produce a connection"));
  if(!job->http_connected){
    if(client->socket_value!=KU_NET_INVALID_SOCKET){
      if(!events)return 0;int pending=0;
      if(ku_net_socket_pending_error(client->socket_value,&pending)==0&&!pending)goto connected;
      ku_net_socket_close(client->socket_value);client->socket_value=KU_NET_INVALID_SOCKET;
    }
    KuTaskNetAddressV1* addresses=(KuTaskNetAddressV1*)job->net_addresses;
    while(job->net_address_index<job->net_address_count){
      KuTaskNetAddressV1* address=&addresses[job->net_address_index++];
      KuNetSocket fd=socket(address->family,address->socktype,address->protocol);if(fd==KU_NET_INVALID_SOCKET)continue;
      if(ku_net_socket_suppress_sigpipe(fd)||ku_net_socket_set_nonblocking(fd)){ku_net_socket_close(fd);continue;}
      if(!ku_net_socket_connect(fd,(const struct sockaddr*)&address->address,address->length)){client->socket_value=fd;goto connected;}
      if(ku_net_socket_error_connecting(ku_net_socket_last_error())){client->socket_value=fd;job->http_interest=KU_TASK_NET_WRITE;return 0;}ku_net_socket_close(fd);
    }
    return ku_task_http_finish(pool,job,__ku_http_client_error("transport","HTTP connection failed"));
  }
connected:
  job->http_connected=1;
  KuHttpClientAsyncIoV1 io={job,ku_task_http_read,ku_task_http_write};
  uint32_t next=__ku_http_client_async_step_v1(job->http_operation,&io,(events&3u)|((events&(KU_TASK_NET_ERROR|KU_TASK_NET_HUP))?3u:0u));
  if(ku_net_now_ms()>=job->net_deadline)return ku_task_http_finish(pool,job,__ku_http_client_error("transport","HTTP request deadline expired"));
  if(next&4u)return ku_task_http_finish(pool,job,(KuError){0});
  if(next&8u){
    ku_drop_net_client(&job->net_client);
    if(job->net_addresses){size_t bytes=job->net_address_count*sizeof(KuTaskNetAddressV1);free(job->net_addresses);job->net_addresses=NULL;job->net_address_count=0;KuTaskBlockingOutputV1 allocation={pool,job,{NULL,NULL},{0,0},KU_TASK_DRIVER_OK,NULL,0,0,0};ku_task_blocking_output_refund(&allocation,bytes);}
    job->net_address_index=0;job->net_resolved=0;job->http_connected=0;job->http_interest=0;return 2;
  }
  if(!(next&3u))return ku_task_http_finish(pool,job,__ku_http_client_error("transport","HTTP operation made no bounded progress"));
  job->http_interest=next&3u;return 0;
}
static uint32_t ku_task_blocking_submit_http(const KuTaskDriverTicketV1* ticket,KuTaskControlV1* control,KuTaskBlockingTokenV1* token,
    KuString method,KuString url,KuString body,uint32_t has_body,KuArray_str names,KuArray_str values,int64_t timeout,int64_t max_body,int64_t config_fault,
    uint64_t parent_deadline,void* context,uint32_t(*reconcile)(void*),KuResult_struct___ku_http_response* output){
  if(!ticket||!ticket->driver||!control||!token||token->pool||!context||!reconcile||!output||names.len!=values.len||names.len>1024u)return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if(config_fault==1){*output=(KuResult_struct___ku_http_response){false,{0},__ku_http_client_error("invalid_config","http.request headers must be object")};return 100u;}
  if(config_fault)return KU_TASK_DRIVER_INVALID_ARGUMENT;
  const char* config_message=NULL;
  uint64_t deadline=ku_net_deadline_after_ms(timeout>=1&&timeout<=300000?(uint32_t)timeout:5000u);
  if(parent_deadline&&parent_deadline<deadline)deadline=parent_deadline;

  KuTaskBlockingPoolV1* pool=(KuTaskBlockingPoolV1*)ticket->driver->blocking_pool;
  if(!pool||pool->initialized!=KU_TASK_DRIVER_STORAGE_LIVE)return KU_TASK_DRIVER_CLOSED;
  if(ku_task_blocking_lock(pool))return KU_TASK_DRIVER_INTERNAL;
  uint32_t status=pool->closing||pool->fault?KU_TASK_DRIVER_CLOSED:pool->resident>=pool->capacity?KU_TASK_DRIVER_LIMIT:KU_TASK_DRIVER_OK;size_t index=0;
  if(status==KU_TASK_DRIVER_OK)status=ku_task_blocking_claim_locked(pool,&index);
  if(status!=KU_TASK_DRIVER_OK){ku_task_blocking_unlock(pool);*output=(KuResult_struct___ku_http_response){false,{0},ku_task_blocking_error("blocking_queue_full","HTTP resolver queue admission rejected")};return KU_TASK_DRIVER_OK;}
  KuTaskBlockingJobV1* job=&pool->jobs[index];job->kind=10u;job->ticket=*ticket;job->http_pool=pool;job->http_control=control;job->net_deadline=deadline;job->net_plain=1;
  *token=(KuTaskBlockingTokenV1){pool,index,0,job->generation};ku_task_blocking_unlock(pool);
  status=ku_task_driver_blocking_register(ticket);int registered=status==KU_TASK_DRIVER_OK;
  if(status==KU_TASK_DRIVER_OK)status=reconcile(context);
  __ku_http_client_header_v1* headers=NULL;
  if(status==KU_TASK_DRIVER_OK&&names.len){
    status=ku_task_http_reserve(job,names.len*sizeof(*headers));
    if(status==KU_TASK_DRIVER_OK){headers=(__ku_http_client_header_v1*)malloc(names.len*sizeof(*headers));if(!headers)status=KU_TASK_BLOCKING_OUT_OF_MEMORY;}
    for(size_t i=0;status==KU_TASK_DRIVER_OK&&i<names.len;i++){
      if(!__ku_http_client_token(names.data[i])){status=100u;config_message="http.request header name is not a valid HTTP token";break;}
      if(!__ku_http_client_header_value(values.data[i])){status=100u;config_message="http.request header contains control characters";break;}
      static const char*const managed[]={"host","content-length","transfer-encoding","connection","proxy-connection","keep-alive","te","trailer","upgrade","expect"};
      for(size_t n=0;n<sizeof(managed)/sizeof(managed[0]);n++)if(__ku_http_client_header_eq(names.data[i],managed[n])){status=100u;config_message="http.request header is managed by the HTTP transport";}
      headers[i]=(__ku_http_client_header_v1){{names.data[i].ptr,names.data[i].len},{values.data[i].ptr,values.data[i].len}};
    }
    if(status==KU_TASK_DRIVER_OK&&names.len>1){qsort(headers,names.len,sizeof(*headers),__ku_http_client_header_compare);for(size_t i=1;i<names.len;i++)if(!__ku_http_client_header_compare(headers+i-1,headers+i)){status=100u;config_message="duplicate http.request header";}}
  }
  if(status==KU_TASK_DRIVER_OK&&(timeout<1||timeout>300000||max_body<1||max_body>16777216)){status=100u;config_message="http config field must be a positive int within its declared limit";}
  if(status==KU_TASK_DRIVER_OK){
    __ku_http_client_input_v1 input={.size=sizeof(input),.flags=has_body,.method={method.ptr,method.len},.url={url.ptr,url.len},.body={body.ptr,body.len},.headers=headers,.header_count=names.len,.timeout_ms=(uint64_t)timeout,.max_body_bytes=(size_t)max_body};
    KuHttpClientAsyncBudgetV1 budget={job,ku_task_http_reserve,ku_task_http_refund};
    if(__ku_http_client_abi_version()!=1)status=KU_TASK_DRIVER_INVALID_ARGUMENT;
    else {uint32_t created=__ku_http_client_async_new_v1(&input,budget,&job->http_operation);if(created)status=created==1?KU_TASK_BLOCKING_OUT_OF_MEMORY:created==2?KU_TASK_DRIVER_INVALID_ARGUMENT:created==3?KU_TASK_DRIVER_LIMIT:KU_TASK_DRIVER_INTERNAL;}
  }
  free(headers);
  if(ku_task_blocking_lock(pool))return KU_TASK_DRIVER_INTERNAL;
  if(status==KU_TASK_DRIVER_OK&&(pool->closing||ku_task_control_atomic_load(&control->phase)!=KU_TASK_CONTROL_LIVE))status=KU_TASK_DRIVER_CLOSED;
  if(status==KU_TASK_DRIVER_OK){
    if(ku_task_driver_lock(ticket->driver)){ku_task_blocking_unlock(pool);return KU_TASK_DRIVER_INTERNAL;}
    KuTaskDriverSlotV1* owner=ku_task_driver_find(ticket);if(owner)job->net_owner=owner->owner_worker;
    int attached=job->net_owner&&(ticket->driver->external_loops||ticket->driver->io_wait);ku_task_driver_unlock(ticket->driver);
    if(!attached)status=KU_TASK_DRIVER_INVALID_ARGUMENT;
  }
  if(status==KU_TASK_DRIVER_OK)status=ku_task_blocking_net_state_locked(pool,job,KU_TASK_BLOCKING_QUEUED);
  if(status==KU_TASK_DRIVER_OK){pool->ring[(pool->head+pool->queued)%pool->capacity]=index;pool->queued++;if(pool->queued>pool->peak_queued)pool->peak_queued=pool->queued;if(pool->accepted!=UINT64_MAX)pool->accepted++;ku_task_blocking_signal(pool);ku_task_blocking_unlock(pool);status=ku_task_driver_set_intent(ticket,KU_TASK_DRIVER_WAIT);return status==KU_TASK_DRIVER_OK?KU_TASK_DRIVER_PENDING:status;}
  ku_task_http_drop_operation(job);
  if(ku_task_blocking_return_locked(pool,job,index)!=KU_TASK_DRIVER_OK){ku_task_blocking_unlock(pool);return KU_TASK_DRIVER_INTERNAL;}ku_task_blocking_unlock(pool);
  if(registered&&ku_task_driver_blocking_retired(ticket)!=KU_TASK_DRIVER_OK)return KU_TASK_DRIVER_INTERNAL;
  *token=(KuTaskBlockingTokenV1){0};uint32_t reconciled=reconcile(context);if(reconciled!=KU_TASK_DRIVER_OK)return reconciled;
  if(status==100u){*output=(KuResult_struct___ku_http_response){false,{0},__ku_http_client_error("invalid_config",config_message?config_message:"HTTP request configuration is invalid")};return 100u;}
  *output=(KuResult_struct___ku_http_response){false,{0},__ku_http_client_error(status==KU_TASK_DRIVER_INVALID_ARGUMENT?"invalid_config":status==KU_TASK_DRIVER_CLOSED?"cancelled":status==KU_TASK_BLOCKING_OUT_OF_MEMORY?"out_of_memory":status==KU_TASK_DRIVER_LIMIT?"memory_limit":"transport","HTTP request preparation rejected")};return KU_TASK_DRIVER_OK;
}
