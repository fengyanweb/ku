/* Plain TCP operations stay on the immutable Task owner. Only DNS and TLS
 * enter the bounded blocking pool. Each readiness snapshot carries a job
 * generation: reuse of an OS descriptor never authorizes a retired job. */
typedef struct KuTaskNetAddressV1 {
  struct sockaddr_storage address;
  size_t length;
  int family,socktype,protocol;
} KuTaskNetAddressV1;
#if defined(_WIN32)
#define KU_TASK_NET_OS_READ POLLRDNORM
#define KU_TASK_NET_OS_WRITE POLLWRNORM
#else
#define KU_TASK_NET_OS_READ POLLIN
#define KU_TASK_NET_OS_WRITE POLLOUT
#endif

static void ku_task_net_resolve(KuTaskBlockingJobV1* job,KuObject* config,
    KuTaskBlockingOutputV1* allocation) {
  KuError error={0};
  uint32_t connect_ms,read_ms,write_ms,max_read;
  KuValue* host=ku_net_config_get(config,"host");
  KuValue* port=ku_net_config_get(config,"port");
  if(!host || host->tag!=KU_STR || !ku_net_host_valid(host->as.s)
      || !port || port->tag!=KU_INT || port->as.i<1 || port->as.i>65535) {
    error=ku_net_error("invalid_config","net.client requires a valid host and port"); goto failed;
  }
  if(!ku_net_config_int(config,"connect_timeout_ms",KU_NET_DEFAULT_TIMEOUT_MS,1,KU_NET_MAX_TIMEOUT_MS,&connect_ms,&error)
      || !ku_net_config_int(config,"read_timeout_ms",KU_NET_DEFAULT_TIMEOUT_MS,1,KU_NET_MAX_TIMEOUT_MS,&read_ms,&error)
      || !ku_net_config_int(config,"write_timeout_ms",KU_NET_DEFAULT_TIMEOUT_MS,1,KU_NET_MAX_TIMEOUT_MS,&write_ms,&error)
      || !ku_net_config_int(config,"max_read_bytes",KU_NET_DEFAULT_MAX_READ_BYTES,1,KU_NET_MAX_READ_BYTES,&max_read,&error)) goto failed;
  if(job->net_present & (uint16_t)((1u<<7)|(1u<<8))) {
    error=ku_net_error("invalid_config","net TLS fields require tls to be true"); goto failed;
  }
  if(ku_net_socket_startup()) { error=ku_net_error("transport_error","network runtime initialization failed"); goto failed; }
  char hostname[KU_NET_MAX_HOST_BYTES+1u],service[6];
  memcpy(hostname,host->as.s.ptr,host->as.s.len);hostname[host->as.s.len]=0;
  (void)snprintf(service,sizeof(service),"%u",(unsigned)port->as.i);
  struct addrinfo hints;memset(&hints,0,sizeof(hints));
  hints.ai_family=AF_UNSPEC;hints.ai_socktype=SOCK_STREAM;hints.ai_protocol=IPPROTO_TCP;
  struct addrinfo* addresses=NULL;
  int status=getaddrinfo(hostname,service,&hints,&addresses);
  if(ku_net_now_ms()>=job->net_deadline) {
    if(addresses)freeaddrinfo(addresses);
    error=ku_net_error("connect_timeout","net connect timed out during host resolution");goto failed;
  }
  if(status || !addresses) {
    if(addresses)freeaddrinfo(addresses);
    error=ku_net_error(status==EAI_MEMORY?"out_of_memory":"connect_error","net host resolution failed");goto failed;
  }
  size_t count=0;
  for(struct addrinfo* at=addresses;at && count<KU_NET_MAX_RESOLVED_ADDRESSES;at=at->ai_next)
    if(at->ai_addr && at->ai_addrlen<=sizeof(struct sockaddr_storage)) count++;
  if(!count) {freeaddrinfo(addresses);error=ku_net_error("connect_error","net host has no supported address");goto failed;}
  KuTaskNetAddressV1* copy=(KuTaskNetAddressV1*)ku_task_blocking_output_allocate(allocation,count*sizeof(*copy));
  if(!copy) {freeaddrinfo(addresses);goto failed;}
  size_t used=0;
  for(struct addrinfo* at=addresses;at && used<count;at=at->ai_next) {
    if(!at->ai_addr || at->ai_addrlen>sizeof(struct sockaddr_storage))continue;
    memset(&copy[used],0,sizeof(copy[used]));memcpy(&copy[used].address,at->ai_addr,at->ai_addrlen);
    copy[used].length=at->ai_addrlen;copy[used].family=at->ai_family;
    copy[used].socktype=at->ai_socktype;copy[used].protocol=at->ai_protocol;used++;
  }
  freeaddrinfo(addresses);
  KuNetClient* client=(KuNetClient*)ku_task_blocking_output_allocate(allocation,sizeof(*client));
  if(!client) {ku_task_blocking_output_release(allocation,copy);goto failed;}
  memset(client,0,sizeof(*client));client->socket_value=KU_NET_INVALID_SOCKET;
  ku_net_atomic_flag_init(&client->poison_requested);
  client->read_timeout_ms=read_ms;client->write_timeout_ms=write_ms;client->max_read_bytes=max_read;
  if(ku_net_gate_init(&client->gate)) {
    ku_task_blocking_output_release(allocation,client);ku_task_blocking_output_release(allocation,copy);
    error=ku_net_error("sync_error","net client synchronization initialization failed");goto failed;
  }
  job->net_client=client;job->net_addresses=copy;job->net_address_count=count;
  job->net_resolved=1;return;
failed:
  job->net_connect_result=(KuResult_net_client){false,NULL,error};
}

/* This function runs outside the pool lock; NET_STEP keeps cancellation and
 * consumption from freeing the exclusively owned socket during one bounded
 * syscall. All publication and budget operations retain pool -> driver order. */
static int ku_task_net_step(KuTaskBlockingPoolV1* pool,KuTaskBlockingJobV1* job,uint32_t events,int cancelled) {
  KuTaskBlockingOutputV1 allocation={pool,job,{NULL,NULL},{0,0},KU_TASK_DRIVER_OK,NULL,0,0,0};
  KuNetClient* client=job->net_client;
  KuError error={0};
  uint64_t now=ku_net_now_ms();
  /* Close is a finite owner release, including cancellation and an expired D. */
  if(job->kind==8u) {ku_drop_net_client(&job->net_client);job->result=(KuResult_null){true,0,{0}};return 1;}
  if(cancelled) {error=ku_task_blocking_error("cancelled","network operation was cancelled");goto failed;}
  if(!job->net_deadline || now>=job->net_deadline) {
    error=ku_net_error(job->kind==5u?"connect_timeout":job->kind==6u?"read_timeout":"write_timeout","network operation timed out");goto failed;
  }
  if(!client || (job->kind!=5u && client->socket_value==KU_NET_INVALID_SOCKET)) {
    error=ku_net_error("closed","net client is closed");goto failed;
  }
  if(job->kind==5u) {
    if(client->socket_value!=KU_NET_INVALID_SOCKET) {
      if(!events)return 0;
      int pending=0;
      if(ku_net_socket_pending_error(client->socket_value,&pending)==0 && !pending)goto connected;
      ku_net_socket_close(client->socket_value);client->socket_value=KU_NET_INVALID_SOCKET;
    }
    KuTaskNetAddressV1* addresses=(KuTaskNetAddressV1*)job->net_addresses;
    /* At most 64 bounded nonblocking attempts, never a synchronous wait. */
    while(job->net_address_index<job->net_address_count) {
      KuTaskNetAddressV1* address=&addresses[job->net_address_index++];
      KuNetSocket fd=socket(address->family,address->socktype,address->protocol);
      if(fd==KU_NET_INVALID_SOCKET)continue;
      if(ku_net_socket_suppress_sigpipe(fd) || ku_net_socket_set_nonblocking(fd)) {ku_net_socket_close(fd);continue;}
      int status=ku_net_socket_connect(fd,(const struct sockaddr*)&address->address,address->length);
      if(!status) {client->socket_value=fd;goto connected;}
      if(ku_net_socket_error_connecting(ku_net_socket_last_error())) {client->socket_value=fd;return 0;}
      ku_net_socket_close(fd);
    }
    error=ku_net_error("connect_error","net connection failed");goto failed;
connected:
    if(ku_net_now_ms()>=job->net_deadline) {error=ku_net_error("connect_timeout","net connect timed out");goto failed;}
    job->net_connect_result=(KuResult_net_client){true,ku_move_net_client(&job->net_client),{0}};return 1;
  }
  if(job->kind==6u) {
    if(job->net_requested<1 || job->net_requested>(int64_t)client->max_read_bytes) {
      error=ku_net_error("invalid_size","net read size is outside configured bounds");goto failed_without_poison;
    }
    if(!(events&(KU_TASK_NET_READ|KU_TASK_NET_ERROR|KU_TASK_NET_HUP)))return 0;
    size_t capacity=(size_t)job->net_requested;
    if(capacity>65536u)capacity=65536u;
    uint8_t* bytes=(uint8_t*)ku_task_blocking_output_allocate(&allocation,capacity);
    if(!bytes)goto allocation_failed;
    int received=ku_net_socket_recv(client->socket_value,bytes,capacity);
    if(received>0) {
      if(ku_net_now_ms()>=job->net_deadline) {ku_task_blocking_output_release(&allocation,bytes);error=ku_net_error("read_timeout","net read timed out");goto failed;}
      job->net_read_result=(KuResult_bytes){true,{bytes,(size_t)received,capacity,KU_BYTES_OWNED},{0}};return 1;
    }
    int failure=ku_net_socket_last_error();
    ku_task_blocking_output_release(&allocation,bytes);
    if(received<0 && (ku_net_socket_error_would_block(failure)||ku_net_socket_error_interrupted(failure)))return 0;
    error=ku_net_error(received==0?"end_of_stream":"read_error",received==0?"net peer closed the connection":"net read failed");goto failed;
  }
  if(job->kind==7u) {
    if(job->net_offset==job->net_write.len) {job->result=(KuResult_null){true,0,{0}};return 1;}
    if(!(events&(KU_TASK_NET_WRITE|KU_TASK_NET_ERROR|KU_TASK_NET_HUP)))return 0;
    size_t length=job->net_write.len-job->net_offset;if(length>65536u)length=65536u;
    int sent=ku_net_socket_send(client->socket_value,job->net_write.ptr+job->net_offset,length);
    if(sent>0) {
      job->net_offset+=(size_t)sent;
      if(ku_net_now_ms()>=job->net_deadline) {error=ku_net_error("write_timeout","net write timed out");goto failed;}
      if(job->net_offset==job->net_write.len) {job->result=(KuResult_null){true,0,{0}};return 1;}
      return 0;
    }
    int failure=ku_net_socket_last_error();
    if(sent<0 && (ku_net_socket_error_would_block(failure)||ku_net_socket_error_interrupted(failure)))return 0;
    error=ku_net_error("write_error","net write failed");goto failed;
  }
  error=ku_net_error("internal_error","invalid owner network operation");goto failed;
allocation_failed:
  error=ku_task_blocking_error(allocation.status==KU_TASK_DRIVER_LIMIT?"memory_limit":allocation.status==KU_TASK_DRIVER_CLOSED?"cancelled":"out_of_memory","network read buffer allocation rejected");
failed:
  if(client)ku_net_poison(client);
failed_without_poison:
  if(job->kind==5u) {ku_drop_net_client(&job->net_client);job->net_connect_result=(KuResult_net_client){false,NULL,error};}
  else if(job->kind==6u)job->net_read_result=(KuResult_bytes){false,{0},error};
  else job->result=(KuResult_null){false,0,error};
  return 1;
}
static uint32_t ku_task_net_dispatch(KuTaskBlockingPoolV1* pool,size_t owner,size_t index,uint64_t generation,uint32_t events) {
  if(!pool || index>=pool->capacity)return KU_TASK_DRIVER_OK;
  if(ku_task_blocking_lock(pool))return KU_TASK_DRIVER_INTERNAL;
  KuTaskBlockingJobV1* job=&pool->jobs[index];
  if(job->generation!=generation || job->state!=KU_TASK_BLOCKING_NET_WAIT || job->net_owner!=owner+1u) {
    ku_task_blocking_unlock(pool);return KU_TASK_DRIVER_OK;
  }
  if(ku_task_blocking_net_state_locked(pool,job,KU_TASK_BLOCKING_NET_STEP)!=KU_TASK_DRIVER_OK) {
    ku_task_blocking_unlock(pool);return KU_TASK_DRIVER_INTERNAL;
  }
  int cancelled=job->cancelled || pool->closing;
  ku_task_blocking_unlock(pool);
#if KU_TASK_BLOCKING_PG
  int done;
  int queue_connect=0;
  if(job->kind==9u) {
    KuPgAsyncOp* operation=job->pg_operation;
    done=ku_pg_async_step(operation,events,ku_pg_now_ms(),cancelled);
    queue_connect=operation->phase==KU_PG_ASYNC_CONNECT && !operation->connect_ready;
    if(done && ku_pg_async_can_dispose(operation)) {
      job->pg_result=ku_pg_async_take(operation);
      ku_pg_async_dispose(operation,1);job->pg_operation=NULL;
    } else if(done) {
      /* A pinned pool notification is still using this stable job. Recheck
       * within one millisecond, bounded by the original request deadline. */
      done=0;
      uint64_t now=ku_pg_now_ms();
      uint64_t cleanup=ku_pg_client_saturating_add_ms(operation->deadline,KU_HANDLER_CLEANUP_GRACE_MS);
      job->net_deadline=now<cleanup ? now+1u:cleanup;
      if(job->net_deadline>cleanup)job->net_deadline=cleanup;
      if(now>=cleanup) {
        /* Preserve the pinned stable job; never free a callback's storage to
         * satisfy a deadline. Report a bounded physical-retirement failure. */
        if(ku_task_blocking_lock(pool))return KU_TASK_DRIVER_INTERNAL;
        pool->fault=KU_TASK_DRIVER_SHUTDOWN_TIMEOUT;pool->closing=1;
        (void)ku_task_blocking_net_state_locked(pool,job,KU_TASK_BLOCKING_NET_WAIT);
        ku_task_blocking_signal(pool);ku_task_blocking_unlock(pool);
        return KU_TASK_DRIVER_SHUTDOWN_TIMEOUT;
      }
    }
  } else done=ku_task_net_step(pool,job,events,cancelled);
#else
  int done=ku_task_net_step(pool,job,events,cancelled);
#endif
  /* Resolver copies are freed when connect completes, before its result can
   * move to a frame. Their receipt is refunded after physical free. */
  if(done && job->net_addresses) {
    size_t bytes=job->net_address_count*sizeof(KuTaskNetAddressV1);
    free(job->net_addresses);job->net_addresses=NULL;job->net_address_count=0;
    KuTaskBlockingOutputV1 output={pool,job,{NULL,NULL},{0,0},KU_TASK_DRIVER_OK,NULL,0,0,0};
    ku_task_blocking_output_refund(&output,bytes);
    if(output.status!=KU_TASK_DRIVER_OK)return output.status;
  }
  if(done) {ku_task_blocking_free(job->net_write.ptr);job->net_write=(KuBytes){0};}
  if(ku_task_blocking_lock(pool))return KU_TASK_DRIVER_INTERNAL;
  job=&pool->jobs[index];
  if(job->generation!=generation || job->state!=KU_TASK_BLOCKING_NET_STEP || job->net_owner!=owner+1u
      || ku_task_blocking_net_state_locked(pool,job,
#if KU_TASK_BLOCKING_PG
        queue_connect ? KU_TASK_BLOCKING_QUEUED :
#endif
        done?KU_TASK_BLOCKING_DONE:KU_TASK_BLOCKING_NET_WAIT)!=KU_TASK_DRIVER_OK) {
    pool->fault=KU_TASK_DRIVER_INTERNAL;ku_task_blocking_unlock(pool);return KU_TASK_DRIVER_INTERNAL;
  }
#if KU_TASK_BLOCKING_PG
  if(queue_connect) {
    pool->ring[(pool->head+pool->queued)%pool->capacity]=index;pool->queued++;
    if(pool->queued>pool->peak_queued)pool->peak_queued=pool->queued;
    ku_task_blocking_signal(pool);
  }
#endif
  if(done) {
    if(pool->finished!=UINT64_MAX)pool->finished++;
    uint32_t status=ku_task_driver_wake(&job->ticket);
    if(status!=KU_TASK_DRIVER_OK)pool->fault=KU_TASK_DRIVER_INTERNAL;
    ku_task_blocking_signal(pool);
  }
  ku_task_blocking_unlock(pool);return KU_TASK_DRIVER_OK;
}
static size_t ku_task_net_interests(KuTaskBlockingPoolV1* pool,size_t owner,KuTaskNetPollV1* items,size_t capacity,uint64_t* deadline) {
  if(!pool)return 0;
  size_t count=0;
  if(ku_task_blocking_lock(pool))return SIZE_MAX;
  if(owner>=KU_TASK_DRIVER_MAX_WORKERS) {ku_task_blocking_unlock(pool);return SIZE_MAX;}
  if(!pool->net_waiting[owner]) {ku_task_blocking_unlock(pool);return 0;}
  for(size_t i=0;i<pool->capacity;i++) {
    KuTaskBlockingJobV1* job=&pool->jobs[i];
    int active=job->state==KU_TASK_BLOCKING_NET_WAIT && job->net_owner==owner+1u;
    if(!active)continue;
    uint64_t generation=job->generation;
#if KU_TASK_BLOCKING_PG
    if(job->kind==9u) {
      KuPgAsyncOp* operation=job->pg_operation;int fd=-1;unsigned int events=0;
      unsigned long long next=operation->deadline;
      int immediate=ku_pg_async_interest(operation,&fd,&events,&next);
      if(operation->phase==KU_PG_ASYNC_DONE && !ku_pg_async_can_dispose(operation)) {
        uint64_t now=ku_pg_now_ms();next=now+1u;immediate=now>=job->net_deadline;
      }
      int advance=job->pg_ready || job->cancelled || pool->closing || ku_pg_now_ms()>=next || immediate;
      job->pg_ready=0;
      if(advance) {
        ku_task_blocking_unlock(pool);
        if(ku_task_net_dispatch(pool,owner,i,generation,0u)>KU_TASK_DRIVER_PENDING)return SIZE_MAX;
        if(ku_task_blocking_lock(pool))return SIZE_MAX;
        job=&pool->jobs[i];
        if(job->generation!=generation || job->state!=KU_TASK_BLOCKING_NET_WAIT)continue;
        operation=job->pg_operation;
        immediate=ku_pg_async_interest(operation,&fd,&events,&next);
        if(operation->phase==KU_PG_ASYNC_DONE && !ku_pg_async_can_dispose(operation))next=job->net_deadline;
        else if(immediate)next=ku_pg_now_ms();
      }
      if(next<*deadline)*deadline=next;
      if(fd>=0 && events) {
        if(count<capacity)items[count++]=(KuTaskNetPollV1){(uintptr_t)(unsigned int)fd,i,job->generation,events};
        else {pool->fault=KU_TASK_DRIVER_LIMIT;ku_task_blocking_unlock(pool);return SIZE_MAX;}
      }
      continue;
    }
#endif
    int advance=job->cancelled || pool->closing || ku_net_now_ms()>=job->net_deadline
        || job->kind==8u || !job->net_client || job->net_client->socket_value==KU_NET_INVALID_SOCKET
        || (job->kind==7u && !job->net_write.len);
    if(advance) {
      /* NET_STEP and its bounded OS operation remain outside the pool lock.
       * Cancellation/consumption can replace this slot before reacquisition. */
      ku_task_blocking_unlock(pool);
      if(ku_task_net_dispatch(pool,owner,i,generation,0u)>KU_TASK_DRIVER_PENDING)return SIZE_MAX;
      if(ku_task_blocking_lock(pool))return SIZE_MAX;
      job=&pool->jobs[i];
    }
    if(job->state==KU_TASK_BLOCKING_NET_WAIT && job->net_owner==owner+1u && job->net_client
        && job->net_client->socket_value!=KU_NET_INVALID_SOCKET) {
      if(job->net_deadline<*deadline)*deadline=job->net_deadline;
      if(count<capacity)items[count++]=(KuTaskNetPollV1){(uintptr_t)job->net_client->socket_value,i,job->generation,
          job->kind==6u?KU_TASK_NET_READ:KU_TASK_NET_WRITE};
      else {pool->fault=KU_TASK_DRIVER_LIMIT;ku_task_blocking_unlock(pool);return SIZE_MAX;}
    }
  }
  ku_task_blocking_unlock(pool);
  return count;
}

typedef struct KuTaskNetWakeV1 { KuNetSocket reader,writer; } KuTaskNetWakeV1;
#if defined(_WIN32)
typedef WSAPOLLFD KuTaskNetOsPollV1;
#else
typedef struct pollfd KuTaskNetOsPollV1;
#endif
typedef struct KuTaskNetRootV1 {
  KuTaskBlockingPoolV1* pool; KuTaskDriverV1* driver;
  size_t count,capacity,scratch_bytes,stride; void* scratch;
  uint32_t reserved,scratch_reserved;KuNetAtomicFlag wake_fault;
  KuTaskNetWakeV1 wakes[KU_TASK_DRIVER_MAX_WORKERS];
} KuTaskNetRootV1;
static size_t ku_task_net_root_fixed_bytes(size_t owners,size_t capacity) {
  if(!owners || owners>KU_TASK_DRIVER_MAX_WORKERS || !capacity || capacity>KU_TASK_NET_POLL_CAPACITY)return SIZE_MAX;
  size_t stride=capacity*sizeof(KuTaskNetPollV1)+(capacity+1u)*sizeof(KuTaskNetOsPollV1);
  if(owners>(SIZE_MAX-sizeof(KuTaskNetRootV1))/stride)return SIZE_MAX;
  return sizeof(KuTaskNetRootV1)+owners*stride;
}
static int ku_task_net_wake_init(KuTaskNetWakeV1* wake) {
  wake->reader=wake->writer=KU_NET_INVALID_SOCKET;
  if(ku_net_socket_startup())return -1;
  wake->reader=socket(AF_INET,SOCK_DGRAM,IPPROTO_UDP);
  wake->writer=socket(AF_INET,SOCK_DGRAM,IPPROTO_UDP);
  if(wake->reader==KU_NET_INVALID_SOCKET || wake->writer==KU_NET_INVALID_SOCKET)goto failed;
  struct sockaddr_in address;memset(&address,0,sizeof(address));address.sin_family=AF_INET;
  address.sin_addr.s_addr=htonl(INADDR_LOOPBACK);
  if(bind(wake->reader,(struct sockaddr*)&address,sizeof(address)))goto failed;
#if defined(_WIN32)
  int length=(int)sizeof(address);
#else
  socklen_t length=(socklen_t)sizeof(address);
#endif
  if(getsockname(wake->reader,(struct sockaddr*)&address,&length)
      || connect(wake->writer,(struct sockaddr*)&address,sizeof(address))
      || ku_net_socket_set_nonblocking(wake->reader) || ku_net_socket_set_nonblocking(wake->writer))goto failed;
  return 0;
failed:
  if(wake->reader!=KU_NET_INVALID_SOCKET)ku_net_socket_close(wake->reader);
  if(wake->writer!=KU_NET_INVALID_SOCKET)ku_net_socket_close(wake->writer);
  wake->reader=wake->writer=KU_NET_INVALID_SOCKET;return -1;
}
static void ku_task_net_root_wake(void* raw,size_t owner) {
  KuTaskNetRootV1* root=(KuTaskNetRootV1*)raw;
  if(owner>=root->count)return;
  const char byte=1;
  /* A full nonblocking datagram socket already has durable readable work. */
  for(size_t attempt=0;attempt<8u;attempt++) {
    int sent=send(root->wakes[owner].writer,&byte,1,0);
    if(sent==1)return;
    int error=ku_net_socket_last_error();
    if(sent<0 && ku_net_socket_error_would_block(error))return;
    if(sent<0 && ku_net_socket_error_interrupted(error))continue;
    /* A fatal wake channel cannot merely set a flag: its owner may already
     * sleep indefinitely. End this failed execution without blocking logs. */
    _Exit(80);
  }
  _Exit(80);
}
static int ku_task_net_root_wait(void* raw,size_t owner,uint64_t deadline) {
  KuTaskNetRootV1* root=(KuTaskNetRootV1*)raw;
  if(ku_net_atomic_flag_load(&root->wake_fault))return -1;
  if(owner>=root->count || !root->scratch)return -1;
  KuTaskNetPollV1* interests=(KuTaskNetPollV1*)((uint8_t*)root->scratch+owner*root->stride);
  uint64_t next=deadline;
  size_t count=ku_task_net_interests(root->pool,owner,interests,root->capacity,&next);
  if(count>root->capacity)return -1;
  KuTaskNetOsPollV1* fds=(KuTaskNetOsPollV1*)((uint8_t*)interests+root->capacity*sizeof(*interests));
  memset(fds,0,(count+1u)*sizeof(*fds));fds[0].fd=root->wakes[owner].reader;fds[0].events=KU_TASK_NET_OS_READ;
  for(size_t i=0;i<count;i++) {fds[i+1].fd=(KuNetSocket)interests[i].socket_value;fds[i+1].events=(short)(((interests[i].events&KU_TASK_NET_READ)?KU_TASK_NET_OS_READ:0)|((interests[i].events&KU_TASK_NET_WRITE)?KU_TASK_NET_OS_WRITE:0));}
  uint64_t now=ku_net_now_ms();int timeout=next==UINT64_MAX?-1:next<=now?0:next-now>(uint64_t)INT_MAX?INT_MAX:(int)(next-now);
#if defined(_WIN32)
  int ready=WSAPoll(fds,(ULONG)(count+1u),timeout);
#else
  int ready=poll(fds,(nfds_t)(count+1u),timeout);
#endif
  if(ready<0)return ku_net_socket_error_interrupted(ku_net_socket_last_error())?0:-1;
  if(fds[0].revents&(POLLERR|POLLNVAL|POLLHUP))return -1;
  if(fds[0].revents&KU_TASK_NET_OS_READ) {char bytes[64];for(size_t i=0;i<64u;i++)if(recv(root->wakes[owner].reader,bytes,sizeof(bytes),0)<=0)break;}
  for(size_t i=0;i<count;i++)if(fds[i+1].revents) {
    uint32_t events=0;short flags=fds[i+1].revents;
    if(flags&KU_TASK_NET_OS_READ)events|=KU_TASK_NET_READ;if(flags&KU_TASK_NET_OS_WRITE)events|=KU_TASK_NET_WRITE;
    if(flags&(POLLERR|POLLNVAL))events|=KU_TASK_NET_ERROR;if(flags&POLLHUP)events|=KU_TASK_NET_HUP;
    if(ku_task_net_dispatch(root->pool,owner,interests[i].slot,interests[i].generation,events)>KU_TASK_DRIVER_PENDING)return -1;
  }
  return 0;
}
static uint32_t ku_task_net_root_attach(KuTaskNetRootV1* root,KuTaskDriverV1* driver,KuTaskBlockingPoolV1* pool) {
  memset(root,0,sizeof(*root));ku_net_atomic_flag_init(&root->wake_fault);root->pool=pool;root->driver=driver;
  size_t fixed=ku_task_net_root_fixed_bytes(driver->workers_created,pool->capacity);
  if(fixed==SIZE_MAX)return KU_TASK_DRIVER_LIMIT;
  root->capacity=pool->capacity;
  root->scratch_bytes=fixed-sizeof(*root);root->stride=root->scratch_bytes/driver->workers_created;
  uint32_t reserved=ku_task_driver_scratch_reserve(driver,root->scratch_bytes);
  if(reserved!=KU_TASK_DRIVER_OK)return reserved;
  root->scratch_reserved=1;
  root->scratch=calloc(1,root->scratch_bytes);
  if(!root->scratch) {
    uint32_t refunded=ku_task_driver_scratch_refund(driver,root->scratch_bytes);root->scratch_reserved=0;
    return refunded==KU_TASK_DRIVER_OK?KU_TASK_BLOCKING_OUT_OF_MEMORY:refunded;
  }
  for(;root->count<driver->workers_created;root->count++)
    if(ku_task_net_wake_init(&root->wakes[root->count])) {
      for(size_t i=0;i<root->count;i++) {ku_net_socket_close(root->wakes[i].reader);ku_net_socket_close(root->wakes[i].writer);}
      root->count=0;free(root->scratch);root->scratch=NULL;
      uint32_t refunded=ku_task_driver_scratch_refund(driver,root->scratch_bytes);root->scratch_reserved=0;
      return refunded==KU_TASK_DRIVER_OK?KU_TASK_DRIVER_INTERNAL:refunded;
    }
  if(ku_task_driver_lock(driver))return KU_TASK_DRIVER_INTERNAL;
  driver->io_context=root;driver->io_wait=ku_task_net_root_wait;
  driver->event_context=root;driver->event_wake=ku_task_net_root_wake;
  ku_task_driver_signal_work(driver);ku_task_driver_unlock(driver);return KU_TASK_DRIVER_OK;
}
static uint32_t ku_task_net_root_destroy(KuTaskNetRootV1* root) {
  for(size_t i=0;i<root->count;i++) {ku_net_socket_close(root->wakes[i].reader);ku_net_socket_close(root->wakes[i].writer);}
  root->count=0;
  free(root->scratch);root->scratch=NULL;
  if(root->scratch_reserved) {
    uint32_t status=ku_task_driver_scratch_refund(root->driver,root->scratch_bytes);
    if(status!=KU_TASK_DRIVER_OK)return status;
    root->scratch_reserved=0;
  }
  return KU_TASK_DRIVER_OK;
}
