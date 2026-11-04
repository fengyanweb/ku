"""Temporary Linux PG call-site mechanism; instrumented, never score eligible."""

BASE = r'''
#if defined(__linux__)
#include <dlfcn.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <sys/syscall.h>
#include <unistd.h>
enum { KU_RS_OWNERS=65, KU_RS_SITES=16, KU_RS_EXAMPLES=2, KU_RS_METHODS=6 };
enum { KU_RS_OTHER=0, KU_RS_PREPARE=1, KU_RS_QUERY_FLUSH_READ=2, KU_RS_RESET_FLUSH_READ=3,
 KU_RS_QUERY_AFTER_FLUSH=4, KU_RS_RESET_AFTER_FLUSH=5, KU_RS_QUERY_READ_EVENT=6,
 KU_RS_RESET_READ_EVENT=7, KU_RS_QUERY_NO_READ_EVENT=8, KU_RS_RESET_NO_READ_EVENT=9,
 KU_RS_QUERY_SEND=10, KU_RS_RESET_SEND=11, KU_RS_QUERY_FLUSH=12, KU_RS_RESET_FLUSH=13,
 KU_RS_QUERY_RESULT=14, KU_RS_RESET_RESULT=15 };
typedef struct {uint64_t step,events,phase,op,call_result,recv_calls,recv_again,recv_bytes;int busy_before;} KuRsExample;
typedef struct {
 uint64_t calls[KU_RS_METHODS],bytes[KU_RS_METHODS],again[KU_RS_METHODS],eof[KU_RS_METHODS],errors[KU_RS_METHODS],eintr[KU_RS_METHODS];
 uint64_t api_calls,api_zero,api_nonzero,busy_before[3],busy_after[2],events[16],examples_count;
 KuRsExample examples[KU_RS_EXAMPLES];
} KuRsSite;
typedef struct {
 KuRsSite sites[KU_RS_SITES];uint64_t steps,step_events[16],phase_in[11],phase_out[11],interest_out[4],done;
 uint64_t busy_before[2],busy_after[2],get_result_null,get_result_present;
 uint64_t epoll_all[3],epoll_task[3],epoll_errors,wait_calls,wait_zero_timeout,wait_nonzero_timeout,wait_empty,wait_ready,wait_error,snapshots[6];
} KuRsOwner;
typedef struct {size_t owner,site;uint64_t step,events,phase,op;int fd,busy;} KuRsContext;
static KuRsOwner ku_rs_owners[KU_RS_OWNERS];
static _Thread_local KuRsContext ku_rs={0,0,0,0,0,0,-1,-1};
static _Thread_local unsigned ku_rs_last_consume_site;
static uint64_t ku_rs_missing_symbols;
extern int PQlibVersion(void);
static int ku_rs_libpq_version;
static ssize_t (*ku_rs_send_real)(int,const void*,size_t,int);
static ssize_t (*ku_rs_recv_real)(int,void*,size_t,int);
static ssize_t (*ku_rs_sendmsg_real)(int,const struct msghdr*,int);
static ssize_t (*ku_rs_recvmsg_real)(int,struct msghdr*,int);
static ssize_t (*ku_rs_write_real)(int,const void*,size_t);
static ssize_t (*ku_rs_read_real)(int,void*,size_t);
static KuRsOwner* ku_rs_owner(void) {return ku_rs.owner && ku_rs.owner<KU_RS_OWNERS?&ku_rs_owners[ku_rs.owner]:NULL;}
static void ku_rs_io(int fd,unsigned method,ssize_t n,int error) {
 KuRsOwner* p=ku_rs_owner();if(!p)return;
 unsigned site=ku_rs.op && fd==ku_rs.fd && ku_rs.site<KU_RS_SITES?(unsigned)ku_rs.site:KU_RS_OTHER;
 KuRsSite* s=&p->sites[site];s->calls[method]++;
 if(n>0)s->bytes[method]+=(uint64_t)n;else if(!n && (method==1u || method==3u || method==5u))s->eof[method]++;
 if(n<0){if(error==EAGAIN || error==EWOULDBLOCK)s->again[method]++;else if(error==EINTR)s->eintr[method]++;else s->errors[method]++;}
}
ssize_t send(int fd,const void* b,size_t n,int flags){ssize_t r=ku_rs_send_real?ku_rs_send_real(fd,b,n,flags):syscall(SYS_sendto,fd,b,n,flags,NULL,0);int e=errno;ku_rs_io(fd,0,r,e);errno=e;return r;}
ssize_t recv(int fd,void* b,size_t n,int flags){ssize_t r=ku_rs_recv_real?ku_rs_recv_real(fd,b,n,flags):syscall(SYS_recvfrom,fd,b,n,flags,NULL,NULL);int e=errno;ku_rs_io(fd,1,r,e);errno=e;return r;}
ssize_t sendmsg(int fd,const struct msghdr* b,int flags){ssize_t r=ku_rs_sendmsg_real?ku_rs_sendmsg_real(fd,b,flags):syscall(SYS_sendmsg,fd,b,flags);int e=errno;ku_rs_io(fd,2,r,e);errno=e;return r;}
ssize_t recvmsg(int fd,struct msghdr* b,int flags){ssize_t r=ku_rs_recvmsg_real?ku_rs_recvmsg_real(fd,b,flags):syscall(SYS_recvmsg,fd,b,flags);int e=errno;ku_rs_io(fd,3,r,e);errno=e;return r;}
ssize_t write(int fd,const void* b,size_t n){ssize_t r=ku_rs_write_real?ku_rs_write_real(fd,b,n):syscall(SYS_write,fd,b,n);int e=errno;ku_rs_io(fd,4,r,e);errno=e;return r;}
ssize_t read(int fd,void* b,size_t n){ssize_t r=ku_rs_read_real?ku_rs_read_real(fd,b,n):syscall(SYS_read,fd,b,n);int e=errno;ku_rs_io(fd,5,r,e);errno=e;return r;}
static void ku_rs_epoll(int task,int operation,int result,int error) {
 KuRsOwner* p=ku_rs_owner();if(!p)return;
 unsigned k=operation==1?0u:operation==3?1u:2u;p->epoll_all[k]++;if(task)p->epoll_task[k]++;if(result<0 && error!=EINTR)p->epoll_errors++;
}
static void ku_rs_wait(int result,int timeout) {KuRsOwner* p=ku_rs_owner();if(!p)return;p->wait_calls++;if(!timeout)p->wait_zero_timeout++;else p->wait_nonzero_timeout++;if(result<0)p->wait_error++;else if(!result)p->wait_empty++;else p->wait_ready+=(uint64_t)result;}
static void ku_rs_snapshot(size_t count){KuRsOwner* p=ku_rs_owner();if(p)p->snapshots[count<3u?count:count<=4u?3u:count<=8u?4u:5u]++;}
static const char* ku_rs_names[KU_RS_SITES]={"other_HTTP_owner","PREPARE_consume","query_FLUSH_backpressure_READ_consume","reset_FLUSH_backpressure_READ_consume","query_READ_same_step_after_FLUSH_consume","reset_READ_same_step_after_FLUSH_consume","query_READ_event_consume","reset_READ_event_consume","query_READ_without_READ_event_consume","reset_READ_without_READ_event_consume","PQsendQueryParams_query","PQsendQueryParams_reset","PQflush_query","PQflush_reset","PQgetResult_query","PQgetResult_reset"};
static void ku_rs_dump(void) {
 for(size_t i=1;i<KU_RS_OWNERS;i++) {KuRsOwner* p=&ku_rs_owners[i];if(!p->steps)continue;
  fprintf(stderr,"KU_MECHANISM {\"kind\":\"pg_io_sites\",\"owner\":%zu,\"NET_STEP\":%llu,\"done\":%llu,\"step_events\":[",i,(unsigned long long)p->steps,(unsigned long long)p->done);
  for(size_t k=0;k<16u;k++)fprintf(stderr,"%s%llu",k?",":"",(unsigned long long)p->step_events[k]);
  fprintf(stderr,"],\"phase_in_0to10\":[");for(size_t k=0;k<11u;k++)fprintf(stderr,"%s%llu",k?",":"",(unsigned long long)p->phase_in[k]);
  fprintf(stderr,"],\"phase_out_0to10\":[");for(size_t k=0;k<11u;k++)fprintf(stderr,"%s%llu",k?",":"",(unsigned long long)p->phase_out[k]);
  fprintf(stderr,"],\"interest_out_0to3\":[");for(size_t k=0;k<4u;k++)fprintf(stderr,"%s%llu",k?",":"",(unsigned long long)p->interest_out[k]);
  fprintf(stderr,"],\"busy_existing_calls\":{\"before_false\":%llu,\"before_true\":%llu,\"after_false\":%llu,\"after_true\":%llu},\"result_null\":%llu,\"result_present\":%llu,\"epoll_all_ADD_MOD_DEL\":[%llu,%llu,%llu],\"epoll_task_ADD_MOD_DEL\":[%llu,%llu,%llu],\"epoll_errors\":%llu,\"wait\":{\"calls\":%llu,\"zero_timeout\":%llu,\"nonzero_timeout\":%llu,\"empty\":%llu,\"ready_events\":%llu,\"error\":%llu},\"net_count_0_1_2_3to4_5to8_over8\":[",(unsigned long long)p->busy_before[0],(unsigned long long)p->busy_before[1],(unsigned long long)p->busy_after[0],(unsigned long long)p->busy_after[1],(unsigned long long)p->get_result_null,(unsigned long long)p->get_result_present,(unsigned long long)p->epoll_all[0],(unsigned long long)p->epoll_all[1],(unsigned long long)p->epoll_all[2],(unsigned long long)p->epoll_task[0],(unsigned long long)p->epoll_task[1],(unsigned long long)p->epoll_task[2],(unsigned long long)p->epoll_errors,(unsigned long long)p->wait_calls,(unsigned long long)p->wait_zero_timeout,(unsigned long long)p->wait_nonzero_timeout,(unsigned long long)p->wait_empty,(unsigned long long)p->wait_ready,(unsigned long long)p->wait_error);
  for(size_t k=0;k<6u;k++)fprintf(stderr,"%s%llu",k?",":"",(unsigned long long)p->snapshots[k]);
  fprintf(stderr,"],\"sites\":[");int sep=0;
  for(size_t k=0;k<KU_RS_SITES;k++) {KuRsSite* s=&p->sites[k];uint64_t count=s->api_calls;for(size_t m=0;m<KU_RS_METHODS;m++)count+=s->calls[m];if(!count)continue;
   fprintf(stderr,"%s{\"site\":%zu,\"name\":\"%s\",\"api_calls\":%llu,\"api_zero\":%llu,\"api_nonzero\":%llu,\"busy_before_unknown_false_true\":[%llu,%llu,%llu],\"existing_busy_after_false_true\":[%llu,%llu],\"events\":[",sep?",":"",k,ku_rs_names[k],(unsigned long long)s->api_calls,(unsigned long long)s->api_zero,(unsigned long long)s->api_nonzero,(unsigned long long)s->busy_before[0],(unsigned long long)s->busy_before[1],(unsigned long long)s->busy_before[2],(unsigned long long)s->busy_after[0],(unsigned long long)s->busy_after[1]);sep=1;
   for(size_t j=0;j<16u;j++)fprintf(stderr,"%s%llu",j?",":"",(unsigned long long)s->events[j]);fprintf(stderr,"],\"IO_send_recv_sendmsg_recvmsg_write_read\":[");
   for(size_t m=0;m<KU_RS_METHODS;m++)fprintf(stderr,"%s{\"calls\":%llu,\"bytes\":%llu,\"EAGAIN\":%llu,\"EOF\":%llu,\"errors\":%llu,\"EINTR\":%llu}",m?",":"",(unsigned long long)s->calls[m],(unsigned long long)s->bytes[m],(unsigned long long)s->again[m],(unsigned long long)s->eof[m],(unsigned long long)s->errors[m],(unsigned long long)s->eintr[m]);
   fprintf(stderr,"],\"examples\":[");for(size_t j=0;j<s->examples_count && j<KU_RS_EXAMPLES;j++){KuRsExample* e=&s->examples[j];fprintf(stderr,"%s{\"step\":%llu,\"events\":%llu,\"phase\":%llu,\"operation\":\"%llx\",\"api_result\":%llu,\"recv_calls\":%llu,\"recv_EAGAIN\":%llu,\"recv_bytes\":%llu,\"busy_before\":%d}",j?",":"",(unsigned long long)e->step,(unsigned long long)e->events,(unsigned long long)e->phase,(unsigned long long)e->op,(unsigned long long)e->call_result,(unsigned long long)e->recv_calls,(unsigned long long)e->recv_again,(unsigned long long)e->recv_bytes,e->busy_before);}fprintf(stderr,"]}");
  }
  fprintf(stderr,"],\"scope\":\"complete service lifetime, not score-only\",\"diagnostic_only\":true,\"normal_score_eligible\":false}\n");
 }
 fprintf(stderr,"KU_MECHANISM {\"kind\":\"pg_io_sites_aggregate\",\"missing_symbols\":%llu,\"libpq_version\":%d,\"fixed_BSS_bytes\":%zu,\"no_added_libpq_progress_calls\":true,\"TLS_IO_visibility_not_proven\":true}\n",(unsigned long long)ku_rs_missing_symbols,ku_rs_libpq_version,sizeof(ku_rs_owners));
}
__attribute__((constructor)) static void ku_rs_init(void) {
 ku_rs_send_real=dlsym(RTLD_NEXT,"send");ku_rs_recv_real=dlsym(RTLD_NEXT,"recv");ku_rs_sendmsg_real=dlsym(RTLD_NEXT,"sendmsg");ku_rs_recvmsg_real=dlsym(RTLD_NEXT,"recvmsg");ku_rs_write_real=dlsym(RTLD_NEXT,"write");ku_rs_read_real=dlsym(RTLD_NEXT,"read");
 ku_rs_missing_symbols=!ku_rs_send_real || !ku_rs_recv_real || !ku_rs_sendmsg_real || !ku_rs_recvmsg_real || !ku_rs_write_real || !ku_rs_read_real;ku_rs_libpq_version=PQlibVersion();atexit(ku_rs_dump);
}
#endif
'''

API = r'''
static KuRsContext ku_rs_begin(KuPgAsyncOp* op,unsigned site) {
 int e=errno;KuRsContext saved=ku_rs;ku_rs.site=site;ku_rs.op=(uint64_t)(uintptr_t)op;ku_rs.phase=(uint64_t)op->phase;ku_rs.fd=op->conn?PQsocket(op->conn):-1;errno=e;return saved;
}
static int ku_rs_send_result(KuRsContext saved,int result) {int e=errno;KuRsOwner* p=ku_rs_owner();if(p){KuRsSite* s=&p->sites[ku_rs.site];s->api_calls++;if(result)s->api_nonzero++;else s->api_zero++;s->events[ku_rs.events&15u]++;s->busy_before[ku_rs.busy<0?0u:ku_rs.busy?2u:1u]++;}ku_rs=saved;errno=e;return result;}
static void ku_rs_end(KuRsContext saved,int result,uint64_t recv_before,uint64_t again_before,uint64_t bytes_before) {
 KuRsOwner* p=ku_rs_owner();if(p && ku_rs.site<KU_RS_SITES) {KuRsSite* s=&p->sites[ku_rs.site];s->api_calls++;if(result)s->api_nonzero++;else s->api_zero++;s->events[ku_rs.events&15u]++;s->busy_before[ku_rs.busy<0?0u:ku_rs.busy?2u:1u]++;
  if(s->examples_count<KU_RS_EXAMPLES){KuRsExample* e=&s->examples[s->examples_count++];*e=(KuRsExample){ku_rs.step,ku_rs.events,ku_rs.phase,ku_rs.op,(uint64_t)(int64_t)result,s->calls[1]+s->calls[3]+s->calls[5]-recv_before,s->again[1]+s->again[3]+s->again[5]-again_before,s->bytes[1]+s->bytes[3]+s->bytes[5]-bytes_before,ku_rs.busy};}
 }ku_rs=saved;
}
#define KU_RS_BEFORE(op,site) KuRsContext saved=ku_rs_begin(op,site);KuRsOwner* p=ku_rs_owner();KuRsSite* s=p?&p->sites[site]:NULL;uint64_t recv_before=s?s->calls[1]+s->calls[3]+s->calls[5]:0,again_before=s?s->again[1]+s->again[3]+s->again[5]:0,bytes_before=s?s->bytes[1]+s->bytes[3]+s->bytes[5]:0
static int ku_rs_consume(KuPgAsyncOp* op,unsigned site) {KU_RS_BEFORE(op,site);int r=PQconsumeInput(op->conn);int e=errno;ku_rs_end(saved,r,recv_before,again_before,bytes_before);ku_rs_last_consume_site=site;errno=e;return r;}
static int ku_rs_flush(KuPgAsyncOp* op) {unsigned site=op->phase>=KU_PG_ASYNC_RESET_SEND?KU_RS_RESET_FLUSH:KU_RS_QUERY_FLUSH;KU_RS_BEFORE(op,site);int r=PQflush(op->conn);int e=errno;ku_rs_end(saved,r,recv_before,again_before,bytes_before);errno=e;return r;}
static PGresult* ku_rs_result(KuPgAsyncOp* op) {unsigned site=op->phase>=KU_PG_ASYNC_RESET_SEND?KU_RS_RESET_RESULT:KU_RS_QUERY_RESULT;KU_RS_BEFORE(op,site);PGresult* r=PQgetResult(op->conn);int e=errno;if(p){if(r)p->get_result_present++;else p->get_result_null++;}ku_rs_end(saved,r!=NULL,recv_before,again_before,bytes_before);errno=e;return r;}
static int ku_rs_busy_result(int result,int after) {int e=errno;KuRsOwner* p=ku_rs_owner();ku_rs.busy=result!=0;if(p){if(after){p->busy_after[result!=0]++;if(ku_rs_last_consume_site<KU_RS_SITES)p->sites[ku_rs_last_consume_site].busy_after[result!=0]++;}else p->busy_before[result!=0]++;}errno=e;return result;}
static void ku_rs_step_start(size_t owner,KuPgAsyncOp* op,unsigned events) {ku_rs.owner=owner;KuRsOwner* p=ku_rs_owner();if(!p)return;p->steps++;p->step_events[events&15u]++;if(op->phase>=0 && op->phase<=10)p->phase_in[op->phase]++;ku_rs.step=p->steps;ku_rs.events=events;ku_rs.busy=-1;ku_rs.site=0;ku_rs.op=0;ku_rs.fd=-1;ku_rs_last_consume_site=0;}
static void ku_rs_step_end(KuPgAsyncOp* op,int done) {KuRsOwner* p=ku_rs_owner();if(p){if(op->phase>=0 && op->phase<=10)p->phase_out[op->phase]++;p->interest_out[op->interest&3u]++;if(done)p->done++;}ku_rs.op=0;ku_rs.fd=-1;ku_rs.site=0;ku_rs.busy=-1;}
#undef KU_RS_BEFORE
'''

def one(source, old, new):
    count=source.count(old)
    if count!=1:raise ValueError(f"PG recv site anchor {count}: {old[:90]}")
    return source.replace(old,new,1)

def transform(source):
    source=one(source,'#include <math.h>','#include <math.h>\n'+BASE)
    source=one(source,'static void ku_pg_async_set_phase(KuPgAsyncOp* op,int phase) {',API+'\nstatic void ku_pg_async_set_phase(KuPgAsyncOp* op,int phase) {')
    source=one(source,'static int ku_pg_async_step_inner(KuPgAsyncOp* op,unsigned int events,unsigned long long now,int cancelled) {','static int ku_pg_async_step_inner(KuPgAsyncOp* op,unsigned int events,unsigned long long now,int cancelled) {\n  int ku_rs_just_flushed=0;')
    source=one(source,'|| !PQconsumeInput(op->conn) || !ku_pg_connection_is_utf8(op->conn))','|| !ku_rs_consume(op,KU_RS_PREPARE) || !ku_pg_connection_is_utf8(op->conn))')
    source=one(source,'if((events&1u) && !PQconsumeInput(op->conn))','if((events&1u) && !ku_rs_consume(op,reset?KU_RS_RESET_FLUSH_READ:KU_RS_QUERY_FLUSH_READ))')
    source=one(source,'int flushed=PQflush(op->conn);','int flushed=ku_rs_flush(op);')
    source=one(source,'      ku_pg_async_set_phase(op,reset ? KU_PG_ASYNC_RESET_READ:KU_PG_ASYNC_READ);','      ku_rs_just_flushed=reset?2:1;\n      ku_pg_async_set_phase(op,reset ? KU_PG_ASYNC_RESET_READ:KU_PG_ASYNC_READ);')
    source=one(source,'\n    if(PQisBusy(op->conn)) {','\n    if(ku_rs_busy_result(PQisBusy(op->conn),0)) {')
    source=one(source,'      if(!PQconsumeInput(op->conn)) {if(reset)goto reset_failed;ku_pg_async_failure(op);return 1;}','      unsigned ku_rs_site=reset?(ku_rs_just_flushed==2?KU_RS_RESET_AFTER_FLUSH:(events&1u)?KU_RS_RESET_READ_EVENT:KU_RS_RESET_NO_READ_EVENT):(ku_rs_just_flushed==1?KU_RS_QUERY_AFTER_FLUSH:(events&1u)?KU_RS_QUERY_READ_EVENT:KU_RS_QUERY_NO_READ_EVENT);\n      ku_rs_just_flushed=0;\n      if(!ku_rs_consume(op,ku_rs_site)) {if(reset)goto reset_failed;ku_pg_async_failure(op);return 1;}')
    source=one(source,'      if(PQisBusy(op->conn)) {op->interest=1u;return 0;}','      if(ku_rs_busy_result(PQisBusy(op->conn),1)) {op->interest=1u;return 0;}')
    source=one(source,'    PGresult* next=PQgetResult(op->conn);','    PGresult* next=ku_rs_result(op);')
    # The exact existing send arguments remain untouched; enter/leave scope only.
    source=one(source,'      op->sent=1;','      op->sent=1;\n      KuRsContext ku_rs_send_scope=ku_rs_begin(op,KU_RS_QUERY_SEND);')
    source=one(source,'if(!PQsendQueryParams(op->conn,op->sql,op->count,','if(!ku_rs_send_result(ku_rs_send_scope,PQsendQueryParams(op->conn,op->sql,op->count,')
    source=one(source,'op->typed?1:0)\n          || !PQsetSingleRowMode(op->conn))','op->typed?1:0))\n          || !PQsetSingleRowMode(op->conn))')
    source=one(source,'    if(op->phase==KU_PG_ASYNC_RESET_SEND) {','    if(op->phase==KU_PG_ASYNC_RESET_SEND) {\n      KuRsContext ku_rs_send_scope;')
    source=one(source,'!PQsendQueryParams(op->conn,"DISCARD ALL",0,NULL,NULL,NULL,NULL,0)','!(ku_rs_send_scope=ku_rs_begin(op,KU_RS_RESET_SEND),ku_rs_send_result(ku_rs_send_scope,PQsendQueryParams(op->conn,"DISCARD ALL",0,NULL,NULL,NULL,NULL,0)))')
    source=one(source,'    done=ku_pg_async_step(operation,events,ku_pg_now_ms(),cancelled);','    ku_rs_step_start(job->net_owner,operation,events);\n    done=ku_pg_async_step(operation,events,ku_pg_now_ms(),cancelled);\n    ku_rs_step_end(operation,done);')
    source=one(source,'  KuHttpLoopOwner* owner = (KuHttpLoopOwner*)argument;','  KuHttpLoopOwner* owner = (KuHttpLoopOwner*)argument;\n  ku_rs.owner=(size_t)owner->index+1u;')
    source=one(source,'do { status=epoll_ctl(r->descriptor,operation,fd,remove?NULL:&event); }','do { status=epoll_ctl(r->descriptor,operation,fd,remove?NULL:&event); int ku_rs_saved_errno=errno;ku_rs_epoll((size_t)(entry-r->registrations)>=(size_t)r->slots+3u && (size_t)(entry-r->registrations)<r->pending_base,operation,status,ku_rs_saved_errno);errno=ku_rs_saved_errno; }')
    source=one(source,'  int selected=epoll_wait(r->descriptor,r->events,(int)KU_HTTP_READY_BATCH,timeout_ms);','  int selected=epoll_wait(r->descriptor,r->events,(int)KU_HTTP_READY_BATCH,timeout_ms);\n  {int ku_rs_saved_errno=errno;ku_rs_wait(selected,timeout_ms);errno=ku_rs_saved_errno;}')
    source=one(source,"    /* The driver's bounded snapshot is compact and may reorder live FDs.","    ku_rs_snapshot(net_count);\n    /* The driver's bounded snapshot is compact and may reorder live FDs.")
    return source
