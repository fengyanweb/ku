"""Temporary, unscored PG owner mechanism transform; remove after observation."""
BASE = r'''
#if defined(__linux__)
#include <dlfcn.h>
#include <unistd.h>
#include <fcntl.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/uio.h>
enum { KU_MP_OWNERS=65, KU_MP_TRACES=64, KU_MP_PAGES=16 };
typedef struct {uint64_t submit,pre_pending,all_pending,first_step,steps;uint64_t send_calls[4],send_bytes[4],send_again[4],recv_calls[4],recv_bytes[4],recv_again[4];} KuMechanismOwner;
typedef struct {uint64_t sequence,kind,owner,slot,generation,jobslot,jobgen,http_slot,http_gen;uintptr_t driver,control;} KuMechanismTrace;
typedef struct {uintptr_t base;size_t cap,item_size,pages,first_write_page,last_write_page;int valid;uint64_t pte[KU_MP_PAGES];unsigned char resident[KU_MP_PAGES];} KuMechanismPages;
typedef struct {size_t owner;KuMechanismPages before,after;} KuMechanismStartup;
static KuMechanismOwner ku_mp_owners[KU_MP_OWNERS];
static KuMechanismTrace ku_mp_traces[KU_MP_TRACES];
static uint64_t ku_mp_sequence,ku_mp_bad_owner,ku_mp_missing_symbol;
static KuMechanismStartup ku_mp_startup[KU_MP_OWNERS];
static _Thread_local size_t ku_mp_owner;
static _Thread_local int ku_mp_pg_fd=-1,ku_mp_phase,ku_mp_wake_read=-1,ku_mp_wake_write=-1;
static int ku_mp_pagemap=-1;
static void ku_mp_inc(uint64_t* p) {__atomic_fetch_add(p,1,__ATOMIC_RELAXED);}
static KuMechanismOwner* ku_mp_get(size_t owner) {if(owner && owner<KU_MP_OWNERS)return &ku_mp_owners[owner];ku_mp_inc(&ku_mp_bad_owner);return NULL;}
static void ku_mp_event(unsigned kind,size_t owner,void* driver,uint64_t slot,uint64_t generation,void* control,size_t jobslot,uint64_t jobgen,uint64_t http_slot,uint64_t http_gen) {
  KuMechanismOwner* p=ku_mp_get(owner);if(!p)return;
  if(kind==1)ku_mp_inc(&p->submit);else if(kind==2)ku_mp_inc(&p->pre_pending);else if(kind==3)ku_mp_inc(&p->first_step);
  uint64_t n=__atomic_fetch_add(&ku_mp_sequence,1,__ATOMIC_RELAXED);
  if(n<KU_MP_TRACES)ku_mp_traces[n]=(KuMechanismTrace){n,kind,owner,slot,generation,jobslot,jobgen,http_slot,http_gen,(uintptr_t)driver,(uintptr_t)control};
}
static ssize_t (*ku_mp_send_real)(int,const void*,size_t,int);
static ssize_t (*ku_mp_recv_real)(int,void*,size_t,int);
static ssize_t (*ku_mp_sendmsg_real)(int,const struct msghdr*,int);
static ssize_t (*ku_mp_recvmsg_real)(int,struct msghdr*,int);
static void ku_mp_io(int fd,int write,ssize_t n,int error) {
  if(!ku_mp_owner || ku_mp_owner>=KU_MP_OWNERS)return;
  unsigned k=fd==ku_mp_pg_fd?(ku_mp_phase>=7?1u:0u):(fd==ku_mp_wake_read || fd==ku_mp_wake_write)?3u:2u;
  KuMechanismOwner* p=&ku_mp_owners[ku_mp_owner];
  uint64_t* calls=write?p->send_calls:p->recv_calls;uint64_t* bytes=write?p->send_bytes:p->recv_bytes;uint64_t* again=write?p->send_again:p->recv_again;
  ku_mp_inc(&calls[k]);if(n>0)__atomic_fetch_add(&bytes[k],(uint64_t)n,__ATOMIC_RELAXED);
  if(n<0 && (error==EAGAIN || error==EWOULDBLOCK))ku_mp_inc(&again[k]);
}
ssize_t send(int fd,const void* b,size_t n,int flags) {if(!ku_mp_send_real){errno=ENOSYS;return -1;}ssize_t r=ku_mp_send_real(fd,b,n,flags);int e=errno;ku_mp_io(fd,1,r,e);errno=e;return r;}
ssize_t recv(int fd,void* b,size_t n,int flags) {if(!ku_mp_recv_real){errno=ENOSYS;return -1;}ssize_t r=ku_mp_recv_real(fd,b,n,flags);int e=errno;ku_mp_io(fd,0,r,e);errno=e;return r;}
ssize_t sendmsg(int fd,const struct msghdr* b,int flags) {if(!ku_mp_sendmsg_real){errno=ENOSYS;return -1;}ssize_t r=ku_mp_sendmsg_real(fd,b,flags);int e=errno;ku_mp_io(fd,1,r,e);errno=e;return r;}
ssize_t recvmsg(int fd,struct msghdr* b,int flags) {if(!ku_mp_recvmsg_real){errno=ENOSYS;return -1;}ssize_t r=ku_mp_recvmsg_real(fd,b,flags);int e=errno;ku_mp_io(fd,0,r,e);errno=e;return r;}
static void ku_mp_pages(KuMechanismPages* out,void* ptr,size_t cap,size_t width) {
  long page=sysconf(_SC_PAGESIZE);out->base=(uintptr_t)ptr;out->cap=cap;out->item_size=width;
  if(page<=0 || !cap || !ptr || width>SIZE_MAX/cap)return;
  uintptr_t first=(uintptr_t)ptr/(size_t)page*(size_t)page,last=((uintptr_t)ptr+cap*width-1)/(size_t)page*(size_t)page;
  size_t pages=(last-first)/(size_t)page+1;out->pages=pages;if(pages>KU_MP_PAGES || ku_mp_pagemap<0)return;
  out->first_write_page=((uintptr_t)ptr+8u-first)/(size_t)page;out->last_write_page=((uintptr_t)ptr+8u+(cap-1u)*width-first)/(size_t)page;
  for(size_t i=0;i<pages;i++) {uint64_t entry=0;unsigned char resident=0;uintptr_t address=first+i*(size_t)page;
    if(pread(ku_mp_pagemap,&entry,sizeof(entry),(off_t)(address/(size_t)page*8u))!=sizeof(entry) || mincore((void*)address,(size_t)page,&resident))return;
    out->pte[i]=entry;out->resident[i]=resident&1u;
  }out->valid=1;
}
static void ku_mp_dump(void) {
  for(size_t i=1;i<KU_MP_OWNERS;i++) {KuMechanismOwner* p=&ku_mp_owners[i];if(!p->submit && !p->steps)continue;
    fprintf(stderr,"KU_MECHANISM {\"kind\":\"pg\",\"owner\":%zu,\"submit\":%llu,\"pre_step_pending_take\":%llu,\"all_pending_take\":%llu,\"first_net_step\":%llu,\"net_steps\":%llu,\"io_categories\":[",i,(unsigned long long)p->submit,(unsigned long long)p->pre_pending,(unsigned long long)p->all_pending,(unsigned long long)p->first_step,(unsigned long long)p->steps);
    for(unsigned k=0;k<4;k++)fprintf(stderr,"%s{\"name\":\"%s\",\"send\":%llu,\"send_bytes\":%llu,\"send_EAGAIN\":%llu,\"recv\":%llu,\"recv_bytes\":%llu,\"recv_EAGAIN\":%llu}",k?",":"",k==0?"PG_query_or_prepare":k==1?"PG_reset":k==2?"other_HTTP_owner_socket":"owner_wakeup",(unsigned long long)p->send_calls[k],(unsigned long long)p->send_bytes[k],(unsigned long long)p->send_again[k],(unsigned long long)p->recv_calls[k],(unsigned long long)p->recv_bytes[k],(unsigned long long)p->recv_again[k]);
    fprintf(stderr,"],\"scope\":\"complete_service_lifetime_not_score_only\",\"not_CPU_evidence\":true}\n");
  }
  uint64_t count=ku_mp_sequence<KU_MP_TRACES?ku_mp_sequence:KU_MP_TRACES;
  for(uint64_t i=0;i<count;i++){KuMechanismTrace* t=&ku_mp_traces[i];fprintf(stderr,"KU_MECHANISM {\"kind\":\"pg_trace\",\"sequence\":%llu,\"event\":%llu,\"owner\":%llu,\"driver\":\"%llx\",\"slot\":%llu,\"generation\":%llu,\"control\":\"%llx\",\"jobslot\":%llu,\"jobgen\":%llu,\"http_slot\":%llu,\"http_generation\":%llu}\n",(unsigned long long)t->sequence,(unsigned long long)t->kind,(unsigned long long)t->owner,(unsigned long long)t->driver,(unsigned long long)t->slot,(unsigned long long)t->generation,(unsigned long long)t->control,(unsigned long long)t->jobslot,(unsigned long long)t->jobgen,(unsigned long long)t->http_slot,(unsigned long long)t->http_gen);}
  for(size_t i=0;i<KU_MP_OWNERS;i++){KuMechanismStartup* s=&ku_mp_startup[i];if(!s->before.base)continue;
    fprintf(stderr,"KU_MECHANISM {\"kind\":\"pending_pages\",\"owner\":%zu,\"base\":\"%llx\",\"cap\":%zu,\"item_size\":%zu,\"before_valid\":%d,\"after_valid\":%d,\"page_records\":[",i,(unsigned long long)s->before.base,s->before.cap,s->before.item_size,s->before.valid,s->after.valid);
    size_t n=s->before.pages<KU_MP_PAGES?s->before.pages:KU_MP_PAGES;for(size_t j=0;j<n;j++)fprintf(stderr,"%s{\"index\":%zu,\"before_present\":%u,\"after_present\":%u,\"before_exclusive\":%u,\"after_exclusive\":%u,\"before_mincore\":%u,\"after_mincore\":%u}",j?",":"",j,(unsigned)(s->before.pte[j]>>63),(unsigned)(s->after.pte[j]>>63),(unsigned)((s->before.pte[j]>>56)&1),(unsigned)((s->after.pte[j]>>56)&1),s->before.resident[j],s->after.resident[j]);
    fprintf(stderr,"],\"object_exclusive_bytes_not_proven\":true,\"instrumented_build_not_normal_RSS_score\":true}\n");}
  fprintf(stderr,"KU_MECHANISM {\"kind\":\"aggregate\",\"trace_events\":%llu,\"traces_kept\":%llu,\"bad_owner\":%llu,\"missing_io_symbol\":%llu,\"not_score\":true}\n",(unsigned long long)ku_mp_sequence,(unsigned long long)count,(unsigned long long)ku_mp_bad_owner,(unsigned long long)ku_mp_missing_symbol);
  if(ku_mp_pagemap>=0)close(ku_mp_pagemap);
}
__attribute__((constructor)) static void ku_mp_init(void) {
  ku_mp_send_real=(ssize_t(*)(int,const void*,size_t,int))dlsym(RTLD_NEXT,"send");ku_mp_recv_real=(ssize_t(*)(int,void*,size_t,int))dlsym(RTLD_NEXT,"recv");
  ku_mp_sendmsg_real=(ssize_t(*)(int,const struct msghdr*,int))dlsym(RTLD_NEXT,"sendmsg");ku_mp_recvmsg_real=(ssize_t(*)(int,struct msghdr*,int))dlsym(RTLD_NEXT,"recvmsg");
  if(!ku_mp_send_real || !ku_mp_recv_real || !ku_mp_sendmsg_real || !ku_mp_recvmsg_real)ku_mp_missing_symbol=1;
  ku_mp_pagemap=open("/proc/self/pagemap",O_RDONLY);atexit(ku_mp_dump);
}
#endif
'''

def _replace(source,old,new):
    if source.count(old)!=1:raise ValueError('PG mechanism anchor '+str(source.count(old))+': '+old[:80])
    return source.replace(old,new,1)

def base_transform(source):
    source=_replace(source,'#include <math.h>','#include <math.h>\n'+BASE)
    site='      job->pg_ready=1;job->net_deadline=operation->deadline;'
    source=_replace(source,site,'      ku_mp_event(1,job->net_owner,ticket->driver,ticket->slot,ticket->generation,control,index,job->generation,operation->waiter.diagnostic.http_task_slot,operation->waiter.diagnostic.http_task_generation);\n'+site)
    site='    if (job->state==KU_TASK_BLOCKING_DONE) {'
    hook='''#if KU_TASK_BLOCKING_PG
    if(!cancel && kind==9u && job->kind==9u && job->state!=KU_TASK_BLOCKING_DONE && job->pg_operation) {
      KuPgAsyncOp* op=job->pg_operation;KuMechanismOwner* diag=ku_mp_get(job->net_owner);if(diag)ku_mp_inc(&diag->all_pending);
      if(!op->diagnostic_steps)ku_mp_event(2,job->net_owner,ticket->driver,ticket->slot,ticket->generation,op->task_phase_context,token->slot,job->generation,op->waiter.diagnostic.http_task_slot,op->waiter.diagnostic.http_task_generation);
    }
#endif
'''
    # Restrict the replacement to take(), not other similarly named branches.
    at=source.index('static uint32_t ku_task_blocking_take(const KuTaskDriverTicketV1* ticket,')
    before,after=source[:at],source[at:];source=before+_replace(after,site,hook+site)
    site='    done=ku_pg_async_step(operation,events,ku_pg_now_ms(),cancelled);'
    hook='''    KuMechanismOwner* diag=ku_mp_get(job->net_owner);if(diag)ku_mp_inc(&diag->steps);
    if(!operation->diagnostic_steps)ku_mp_event(3,job->net_owner,job->ticket.driver,job->ticket.slot,job->ticket.generation,operation->task_phase_context,index,job->generation,operation->waiter.diagnostic.http_task_slot,operation->waiter.diagnostic.http_task_generation);
    ku_mp_owner=job->net_owner;ku_mp_pg_fd=operation->conn?PQsocket(operation->conn):-1;ku_mp_phase=operation->phase;
'''
    source=_replace(source,site,hook+site+'\n    ku_mp_owner=job->net_owner;ku_mp_pg_fd=-1;ku_mp_phase=0;')
    # Keep the current phase accurate as one bounded NET_STEP advances query/reset.
    site='    op->interest=0;\n    if(op->row) {'
    source=_replace(source,site,'    ku_mp_phase=op->phase;ku_mp_pg_fd=op->conn?PQsocket(op->conn):-1;\n'+site)
    site='  ku_http_pending_init(&pending,&active);'
    hook='''  size_t ku_mp_start_owner=(size_t)owner->index;
  if(ku_mp_start_owner<KU_MP_OWNERS)ku_mp_pages(&ku_mp_startup[ku_mp_start_owner].before,pending.items,(size_t)pending.cap,sizeof(*pending.items));
'''
    source=_replace(source,site,hook+site+'\n  if(ku_mp_start_owner<KU_MP_OWNERS)ku_mp_pages(&ku_mp_startup[ku_mp_start_owner].after,pending.items,(size_t)pending.cap,sizeof(*pending.items));')
    source=_replace(source,'  op->phase=phase;','  op->phase=phase; if(ku_mp_owner) {ku_mp_phase=phase;ku_mp_pg_fd=op->conn?PQsocket(op->conn):-1;}')
    source=_replace(source,'  ku_http_current_loop = owner;','  ku_http_current_loop = owner; ku_mp_owner=(size_t)owner->index+1u;')
    source=_replace(source,'  owner->wake = &wake; owner->handler_queue=&queue; owner->ready = !listen_failure;','  owner->wake = &wake; owner->handler_queue=&queue; owner->ready = !listen_failure; ku_mp_wake_read=(int)wake.read_socket;ku_mp_wake_write=(int)wake.write_socket;')
    return source


"""Temporary existing-C transform for the one joint mechanism observation."""
SUPPORT = '#if defined(KU_HTTP_HAS_TASK_NET)\n#define KU_NET_PROBE_MAX_OWNERS 64u\n/* Diagnostic-only, Linux joint PG mechanism window. Not a scored product.\n * Owner-local records. Allocate once on owner entry; no per-turn malloc/logging.\n * Parent dumps/frees map pointers only after the HTTP owner threads join. */\nenum { KU_NP_REORDER=1u,KU_NP_GENERATION=2u,KU_NP_FD=4u,\n       KU_NP_INTEREST=8u,KU_NP_ABSENT=16u,KU_NP_NEW_UNKNOWN=32u };\ntypedef struct { uint64_t stamp; size_t position; unsigned cause,seen_ever; } KuNetProbeSlot;\ntypedef struct {size_t slot,old_position,new_position;uint64_t generation,token;long long fd;unsigned events;} KuNetProbeSwap;\ntypedef struct {\n  uint64_t hypothetical_pages[64];uint64_t distinct_slots,max_slot,min_slot,hypothetical_page_count,page_counter_unknown,compact_position_pages_upper;size_t page_size;\n  KuNetProbeSwap samples[16];size_t sample_count;uint64_t reordered_stamp;\n  KuNetProbeSlot* slots;size_t capacity;uint64_t stamp;\n  uint64_t snapshots,hist[6],max_count,reordered_snapshots,overflow,bad_slot,duplicate_slot,allocation_failed;\n  uint64_t deletes[64],adds[64],interest_mods,kernel_add,kernel_mod,kernel_del,kernel_errors,kernel_eintr;\n} KuNetProbeOwner;\nstatic KuNetProbeOwner ku_net_probe[KU_NET_PROBE_MAX_OWNERS];\nstatic _Thread_local KuNetProbeOwner* ku_net_probe_current;\nstatic void ku_net_probe_inc(uint64_t* value) {if(*value!=UINT64_MAX)(*value)++;}\nstatic void ku_net_probe_begin_owner(size_t owner,size_t capacity) {\n  if(owner>=KU_NET_PROBE_MAX_OWNERS)return;\n  KuNetProbeOwner* p=&ku_net_probe[owner];ku_net_probe_current=p;\n  if(p->slots || p->capacity)return;\n  long page=sysconf(_SC_PAGESIZE);p->page_size=page>0?(size_t)page:0u;\n  p->capacity=capacity;p->slots=calloc(capacity,sizeof(*p->slots));\n  if(!p->slots)ku_net_probe_inc(&p->allocation_failed);\n}\nstatic void ku_net_probe_snapshot(KuHttpReactor* r,const KuTaskNetPollV1* items,size_t count) {\n  KuNetProbeOwner* p=ku_net_probe_current;if(!p)return;\n  ku_net_probe_inc(&p->snapshots);if(count>p->max_count)p->max_count=count;\n  size_t bin=count<3u?count:count<=4u?3u:count<=8u?4u:5u;ku_net_probe_inc(&p->hist[bin]);\n  if(!p->slots || p->stamp==UINT64_MAX) {ku_net_probe_inc(&p->overflow);return;}\n  p->stamp++;\n  if(p->page_size && count) {\n    uintptr_t first=(uintptr_t)&r->registrations[(size_t)r->slots+3u];\n    uint64_t pages=(uint64_t)((first+count*sizeof(KuHttpRegistration)-1u)/p->page_size-first/p->page_size+1u);\n    if(pages>p->compact_position_pages_upper)p->compact_position_pages_upper=pages;\n  }\n  for(size_t i=0;i<count;i++) {\n    size_t slot=items[i].slot;if(slot>=p->capacity) {ku_net_probe_inc(&p->bad_slot);continue;}\n    KuNetProbeSlot* s=&p->slots[slot];if(s->stamp==p->stamp)ku_net_probe_inc(&p->duplicate_slot);\n    if(!s->seen_ever) {\n      s->seen_ever=1;if(!p->distinct_slots)p->min_slot=slot;\n      if(slot<p->min_slot)p->min_slot=slot;if(slot>p->max_slot)p->max_slot=slot;ku_net_probe_inc(&p->distinct_slots);\n      size_t index=(size_t)r->slots+3u+slot;\n      if(!p->page_size || index>=r->pending_base || index>=r->capacity)ku_net_probe_inc(&p->page_counter_unknown);\n      else {\n        uintptr_t address=(uintptr_t)&r->registrations[index],base=(uintptr_t)r->registrations;\n        size_t first=(size_t)(address/p->page_size-base/p->page_size);\n        size_t last=(size_t)((address+sizeof(KuHttpRegistration)-1u)/p->page_size-base/p->page_size);\n        for(size_t page=first;page<=last;page++) {\n          if(page>=4096u) {ku_net_probe_inc(&p->page_counter_unknown);break;}\n          uint64_t mask=UINT64_C(1)<<(page%64u);if(!(p->hypothetical_pages[page/64u]&mask)) {p->hypothetical_pages[page/64u]|=mask;ku_net_probe_inc(&p->hypothetical_page_count);}\n        }\n      }\n    }\n    s->stamp=p->stamp;s->position=i;s->cause=0;\n  }\n}\nstatic short ku_net_probe_events(uint32_t events) {\n  return (short)(((events&KU_TASK_NET_READ)?KU_HTTP_POLL_READ:0)|((events&KU_TASK_NET_WRITE)?KU_HTTP_POLL_WRITE:0));\n}\nstatic unsigned ku_net_probe_delete_reason(const KuHttpRegistration* old,size_t previous_position,\n    const KuTaskNetPollV1* items,size_t count) {\n  KuNetProbeOwner* p=ku_net_probe_current;\n  if(!p || !p->slots || !p->stamp || old->resource_slot>=p->capacity)return KU_NP_NEW_UNKNOWN;\n  KuNetProbeSlot* s=&p->slots[old->resource_slot];\n  if(s->stamp!=p->stamp || s->position>=count)return KU_NP_ABSENT;\n  const KuTaskNetPollV1* now=&items[s->position];unsigned reason=0;\n  if(s->position!=previous_position)reason|=KU_NP_REORDER;\n  if(old->generation!=now->generation)reason|=KU_NP_GENERATION;\n  if(old->fd!=(KuHttpSocket)now->socket_value)reason|=KU_NP_FD;\n  if(old->events!=ku_net_probe_events(now->events))reason|=KU_NP_INTEREST;\n  return reason;\n}\nstatic void ku_net_probe_before_delete(const KuHttpRegistration* old,size_t previous_position,\n    const KuTaskNetPollV1* items,size_t count) {\n  KuNetProbeOwner* p=ku_net_probe_current;if(!p || !old->registered)return;\n  unsigned reason=ku_net_probe_delete_reason(old,previous_position,items,count);\n  ku_net_probe_inc(&p->deletes[reason]);\n  if(reason==KU_NP_REORDER) {\n    if(p->reordered_stamp!=p->stamp) {ku_net_probe_inc(&p->reordered_snapshots);p->reordered_stamp=p->stamp;}\n    if(p->sample_count<16u) {\n      KuNetProbeSlot* now=&p->slots[old->resource_slot];\n      p->samples[p->sample_count++]=(KuNetProbeSwap){old->resource_slot,previous_position,now->position,old->generation,old->token,(long long)old->fd,(unsigned)old->events};\n    }\n  }\n  if(p->slots && old->resource_slot<p->capacity) {\n    KuNetProbeSlot* s=&p->slots[old->resource_slot];if(s->stamp==p->stamp)s->cause=reason;\n  }\n}\nstatic void ku_net_probe_before_interest(const KuHttpRegistration* old,const KuTaskNetPollV1* now) {\n  KuNetProbeOwner* p=ku_net_probe_current;if(!p)return;\n  short events=ku_net_probe_events(now->events);\n  if(old->registered && old->fd==(KuHttpSocket)now->socket_value && old->generation==now->generation) {\n    if(old->events!=events)ku_net_probe_inc(&p->interest_mods);return;\n  }\n  unsigned reason=KU_NP_NEW_UNKNOWN;\n  if(p->slots && now->slot<p->capacity) {\n    KuNetProbeSlot* s=&p->slots[now->slot];if(s->stamp==p->stamp && s->cause)reason=s->cause;\n  }\n  ku_net_probe_inc(&p->adds[reason]);\n}\nstatic int ku_net_probe_is_task_entry(KuHttpReactor* r,KuHttpRegistration* entry) {\n  uintptr_t base=(uintptr_t)r->registrations,address=(uintptr_t)entry;\n  if(address<base || (address-base)%sizeof(*entry))return 0;\n  size_t index=(size_t)((address-base)/sizeof(*entry));\n  return index>=(size_t)r->slots+3u && index<r->pending_base;\n}\nstatic void ku_net_probe_kernel_attempt(KuHttpReactor* r,KuHttpRegistration* entry,int operation) {\n  KuNetProbeOwner* p=ku_net_probe_current;if(!p || !ku_net_probe_is_task_entry(r,entry))return;\n#if defined(__linux__)\n  if(operation==EPOLL_CTL_ADD)ku_net_probe_inc(&p->kernel_add);\n  else if(operation==EPOLL_CTL_MOD)ku_net_probe_inc(&p->kernel_mod);\n  else if(operation==EPOLL_CTL_DEL)ku_net_probe_inc(&p->kernel_del);\n#else\n  (void)operation;\n#endif\n}\nstatic void ku_net_probe_kernel_result(KuHttpReactor* r,KuHttpRegistration* entry,int result,int error) {\n  KuNetProbeOwner* p=ku_net_probe_current;\n  if(!p || !ku_net_probe_is_task_entry(r,entry) || result>=0)return;\n  ku_net_probe_inc(&p->kernel_errors);if(error==EINTR)ku_net_probe_inc(&p->kernel_eintr);\n}\n\nstatic void ku_net_probe_dump(void) {\n  for(size_t owner=0;owner<KU_NET_PROBE_MAX_OWNERS;owner++) {\n    KuNetProbeOwner* p=&ku_net_probe[owner];if(!p->capacity && !p->snapshots)continue;\n    fprintf(stderr,"KU_MECHANISM {\\"kind\\":\\"net\\",\\"owner\\":%zu,\\"capacity\\":%zu,\\"snapshots\\":%llu,\\"max_net_count\\":%llu,\\"reorder_snapshots\\":%llu,\\"hist_0_1_2_3to4_5to8_over8\\":[",owner,p->capacity,(unsigned long long)p->snapshots,(unsigned long long)p->max_count,(unsigned long long)p->reordered_snapshots);\n    for(size_t i=0;i<6u;i++)fprintf(stderr,"%s%llu",i?",":"",(unsigned long long)p->hist[i]);\n    fprintf(stderr,"],\\"distinct_job_slots\\":%llu,\\"min_job_slot\\":%llu,\\"max_job_slot\\":%llu,\\"registration_bytes\\":%zu,\\"map_record_bytes\\":%zu,\\"page_size\\":%zu,\\"hypothetical_stable_pages_upper\\":%llu,\\"hypothetical_stable_bytes_upper\\":%llu,\\"compact_position_pages_upper\\":%llu,\\"page_counter_unknown\\":%llu,\\"page_bounds_are_not_RSS\\":true,\\"kernel_add\\":%llu,\\"kernel_mod\\":%llu,\\"kernel_del\\":%llu,\\"kernel_errors\\":%llu,\\"kernel_eintr\\":%llu,\\"interest_mods\\":%llu,\\"overflow\\":%llu,\\"bad_slot\\":%llu,\\"duplicate_slot\\":%llu,\\"allocation_failed\\":%llu,\\"delete_causes\\":{",(unsigned long long)p->distinct_slots,(unsigned long long)p->min_slot,(unsigned long long)p->max_slot,sizeof(KuHttpRegistration),sizeof(KuNetProbeSlot),p->page_size,(unsigned long long)p->hypothetical_page_count,(unsigned long long)(p->hypothetical_page_count*p->page_size),(unsigned long long)p->compact_position_pages_upper,(unsigned long long)p->page_counter_unknown,(unsigned long long)p->kernel_add,(unsigned long long)p->kernel_mod,(unsigned long long)p->kernel_del,(unsigned long long)p->kernel_errors,(unsigned long long)p->kernel_eintr,(unsigned long long)p->interest_mods,(unsigned long long)p->overflow,(unsigned long long)p->bad_slot,(unsigned long long)p->duplicate_slot,(unsigned long long)p->allocation_failed);\n    int separator=0;for(size_t i=0;i<64u;i++)if(p->deletes[i]) {fprintf(stderr,"%s\\"%zu\\":%llu",separator?",":"",i,(unsigned long long)p->deletes[i]);separator=1;}\n    fprintf(stderr,"},\\"add_causes\\":{");separator=0;\n    for(size_t i=0;i<64u;i++)if(p->adds[i]) {fprintf(stderr,"%s\\"%zu\\":%llu",separator?",":"",i,(unsigned long long)p->adds[i]);separator=1;}\n    fprintf(stderr,"},\\"first_swaps\\":[");\n    for(size_t i=0;i<p->sample_count;i++) {KuNetProbeSwap* q=&p->samples[i];fprintf(stderr,"%s{\\"slot\\":%zu,\\"old_position\\":%zu,\\"new_position\\":%zu,\\"generation\\":%llu,\\"token\\":%llu,\\"fd\\":%lld,\\"events\\":%u}",i?",":"",q->slot,q->old_position,q->new_position,(unsigned long long)q->generation,(unsigned long long)q->token,q->fd,q->events);}\n    fprintf(stderr,"],\\"absence_means_retirement\\":false,\\"diagnostic_not_score\\":true}\\n");free(p->slots);p->slots=NULL;\n  }\n}\n__attribute__((constructor)) static void ku_net_probe_register_dump(void) {atexit(ku_net_probe_dump);}\n\n#else\n#define ku_net_probe_kernel_attempt(...) ((void)0)\n#define ku_net_probe_kernel_result(...) ((void)0)\n#endif\n'

def _one(source,old,new):
    if source.count(old)!=1:raise ValueError("net probe anchor count: "+old[:90])
    return source.replace(old,new,1)

def net_transform(source):
    anchor="static size_t ku_http_reactor_hash_capacity(size_t capacity) {"
    source=_one(source,anchor,SUPPORT+"\n"+anchor)
    old='  if(!net_capacity || net_capacity>KU_TASK_NET_POLL_CAPACITY)return "Task network capacity invalid";'
    source=_one(source,old,old+'\n  ku_net_probe_begin_owner((size_t)owner->index,net_capacity);')
    old="    /* The driver's bounded snapshot is compact and may reorder live FDs."
    source=_one(source,old,'    ku_net_probe_snapshot(&reactor,net_interests,net_count);\n'+old)
    old="""if(i>=net_count || entry->fd!=poll_items[net_base+(int)i].fd
          || entry->generation!=net_interests[i].generation || entry->resource_slot!=net_interests[i].slot)
        ku_http_reactor_delete(&reactor,(size_t)slot_count+3u+i);"""
    new="""if(i>=net_count || entry->fd!=poll_items[net_base+(int)i].fd
          || entry->generation!=net_interests[i].generation || entry->resource_slot!=net_interests[i].slot) {
        ku_net_probe_before_delete(entry,i,net_interests,net_count);
        ku_http_reactor_delete(&reactor,(size_t)slot_count+3u+i);
      }"""
    source=_one(source,old,new)
    old="      ku_http_reactor_interest(&reactor,(size_t)slot_count+3u+i,poll_items[net_base+(int)i].fd,"
    source=_one(source,old,'      ku_net_probe_before_interest(&reactor.registrations[(size_t)slot_count+3u+i],&net_interests[i]);\n'+old)
    old="do { status=epoll_ctl(r->descriptor,operation,fd,remove?NULL:&event); }"
    new="do { ku_net_probe_kernel_attempt(r,entry,operation); status=epoll_ctl(r->descriptor,operation,fd,remove?NULL:&event); ku_net_probe_kernel_result(r,entry,status,errno); }"
    return _one(source,old,new)


def transform(source):
    derived=net_transform(base_transform(source))
    # Allocate diagnostic net map only after pending's before/after snapshots.
    old='  ku_net_probe_begin_owner((size_t)owner->index,net_capacity);'
    derived=_one(derived,old,'')
    anchor='  if(ku_mp_start_owner<KU_MP_OWNERS)ku_mp_pages(&ku_mp_startup[ku_mp_start_owner].after,pending.items,(size_t)pending.cap,sizeof(*pending.items));'
    return _one(derived,anchor,anchor+'\n'+old)
