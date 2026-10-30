/* Included after an unchanged normal CLI-generated server and generated cases.
 * This is a synchronous diagnostic, never an HTTP/Rust acceptance score. */
#define KU_JSON_DENSE_BATCH 2048u
#define KU_JSON_DENSE_MAX_RESPONSE ((size_t)1048576u)

static int ku_json_dense_clock(clockid_t clock,uint64_t* out) {
  struct timespec value={0};
  if(clock_gettime(clock,&value) || value.tv_sec<0 || value.tv_nsec<0
      || value.tv_nsec>=1000000000L || (uint64_t)value.tv_sec>UINT64_MAX/1000000000u)return 0;
  uint64_t seconds=(uint64_t)value.tv_sec*1000000000u;
  if((uint64_t)value.tv_nsec>UINT64_MAX-seconds)return 0;
  *out=seconds+(uint64_t)value.tv_nsec;return 1;
}
static int ku_json_dense_clone(const uint8_t* bytes,size_t length,KuString* out) {
  *out=(KuString){0};
  if(!length)return 1;
  uint8_t* copy=(uint8_t*)malloc(length);if(!copy)return 0;
  memcpy(copy,bytes,length);*out=(KuString){copy,length,length,KU_STRING_OWNED};return 1;
}
static int ku_json_dense_call(const KuJsonDenseCase* input,int http_context,
    uint64_t request_ms,uint64_t handler_ms,int headers,KuString* output) {
#if KU_JSON_DENSE_PHASE_LEDGER
  unsigned phase_outer=ku_dense_change_phase(KU_DENSE_REQUEST);
#endif
  KuStruct___ku_http_request request={0};KuString rid={0},key={0};
  *output=(KuString){0};
  /* The real request deadline begins at socket admission/first request bytes.
   * Dense has no socket or queue: create that deadline immediately before its
   * owned projection, then preserve job_run's expiry gate and job_begin's clamp.
   * Job locks, queue wakeups, routing and buffer accounting are NOT simulated. */
  unsigned long long request_deadline=http_context
      ? ku_http_deadline_ms((uint32_t)request_ms):0;
  if(http_context && ku_http_now_ms()>=request_deadline)return 0;
  __ku_sync_reset();
  if(!ku_json_dense_clone(input->body,input->body_length,&request.body)
      || !ku_object_try_new(0,&request.query)
      || !ku_json_dense_clone(input->rid,input->rid_length,&rid)
      || !ku_json_dense_clone((const uint8_t*)"rid",3u,&key))goto prepare_failed;
  KuValue value=ku_v_str(ku_string_move(&rid));
  if(!ku_object_try_set_owned(request.query,&key,&value)) {
    ku_value_drop(&value);goto prepare_failed;
  }
  if(http_context) {
    unsigned long long now=ku_http_now_ms();
    unsigned long long remaining=request_deadline>now ? request_deadline-now:0;
    /* timeout_begin takes a RELATIVE duration, not the absolute deadline. It
     * obtains its own monotonic time, exactly as the normal worker path does. */
    __ku_handler_timeout_begin(handler_ms>remaining ? remaining:handler_ms);
  }
  /* encode takes the request by value and retires every request field on both
   * normal/error returns. The local copy is no longer an owner after this call. */
  KuResult_struct___ku_http_response result=encode(request);
  request=(KuStruct___ku_http_request){0};
#if KU_JSON_DENSE_PHASE_LEDGER
  ku_dense_change_phase(KU_DENSE_VALIDATE);
#endif
  KuSyncExitSignal signal={0};
  int expired=__ku_sync_finish_request(&signal);
  static const char content_type[]="text/plain; charset=utf-8";
  int valid=result.ok && !expired && signal.kind==KU_SYNC_EXIT_NONE
      && !__ku_call_depth && result.value.status==200
      && result.value.body.len<=KU_JSON_DENSE_MAX_RESPONSE
      && (!result.value.body.len || result.value.body.ptr)
      && (!headers || (result.value.content_type.len==sizeof(content_type)-1u
          && !memcmp(result.value.content_type.ptr,content_type,sizeof(content_type)-1u)));
  if(valid)*output=ku_string_move(&result.value.body);
  ku_result_drop_struct___ku_http_response(&result);
#if KU_JSON_DENSE_PHASE_LEDGER
  ku_dense_change_phase(phase_outer);
#endif
  return valid;
prepare_failed:
  ku_string_drop(&key);ku_string_drop(&rid);ku_drop_struct___ku_http_request(&request);
#if KU_JSON_DENSE_PHASE_LEDGER
  ku_dense_change_phase(phase_outer);
#endif
  __ku_sync_reset();return 0;
}
static int ku_json_dense_unsigned(const char* text,uint64_t* result) {
  if(!text || *text<'0' || *text>'9')return 0;
  char* end=NULL;errno=0;unsigned long long value=strtoull(text,&end,10);
  if(errno || !end || *end)return 0;*result=(uint64_t)value;return 1;
}
static int ku_json_dense_real(const char* text,double* result) {
  char* end=NULL;errno=0;double value=strtod(text,&end);
  if(errno || !end || end==text || *end || !isfinite(value))return 0;
  *result=value;return 1;
}
static void ku_json_dense_hex(KuString bytes) {
  for(size_t i=0;i<bytes.len;i++)fprintf(stdout,"%02x",(unsigned)bytes.ptr[i]);
}
/* Called only after the measured loop AND golden retirement. Use names shared
 * by old A and Q1/Q2 candidates. The emitter identifies inline/cold-plane input
 * storage; preprocessor guards never name a type/member absent in old A. */
static void ku_json_dense_layout(void) {
  fputs("{\"schema\":\"ku-json-dense-layout-v1\",\"resident_memory_measurement\":false,",stdout);
  fprintf(stdout,"\"sizeof\":{\"void_pointer\":%zu,\"KuString\":%zu,\"KuValue\":%zu",
      sizeof(void*),sizeof(KuString),sizeof(KuValue));
#if defined(KU_TASK_BLOCKING_FILES) && defined(KU_TASK_BLOCKING_DIRECTORY) && defined(KU_TASK_BLOCKING_NETWORK) && defined(KU_TASK_BLOCKING_PG)
  fprintf(stdout,",\"KuTaskBlockingJobV1\":%zu,\"KuTaskBlockingPoolV1\":%zu,\"ring_cell\":%zu,\"net_config\":",
      sizeof(KuTaskBlockingJobV1),sizeof(KuTaskBlockingPoolV1),sizeof(size_t));
#if KU_TASK_BLOCKING_NETWORK
#if KU_JSON_DENSE_HAS_COLD_INPUTS
  fprintf(stdout,"%zu",sizeof(((KuTaskBlockingInputsV1*)0)->net_config));
#else
  fprintf(stdout,"%zu",sizeof(((KuTaskBlockingJobV1*)0)->net_config));
#endif
#else
  fputs("null",stdout);
#endif
  fputs(",\"KuTaskBlockingInputsV1\":",stdout);
#if KU_JSON_DENSE_HAS_COLD_INPUTS
  fprintf(stdout,"%zu",sizeof(KuTaskBlockingInputsV1));
#else
  fputc('0',stdout);
#endif
  fputs(",\"free_index_cell\":",stdout);
#if KU_JSON_DENSE_HAS_FREE_INDEX
  fprintf(stdout,"%zu",sizeof(uint32_t));
#else
  fputc('0',stdout);
#endif
  fprintf(stdout,"},\"available\":true,\"features\":{\"KU_TASK_BLOCKING_FILES\":%d,\"KU_TASK_BLOCKING_DIRECTORY\":%d,\"KU_TASK_BLOCKING_NETWORK\":%d,\"KU_TASK_BLOCKING_PG\":%d},",
      KU_TASK_BLOCKING_FILES,KU_TASK_BLOCKING_DIRECTORY,KU_TASK_BLOCKING_NETWORK,KU_TASK_BLOCKING_PG);
  fprintf(stdout,"\"offsetof\":{\"generation\":%zu,\"output_charge\":%zu,\"ticket\":%zu,\"result\":%zu,\"exists_value\":%zu",
      offsetof(KuTaskBlockingJobV1,generation),offsetof(KuTaskBlockingJobV1,output_charge),
      offsetof(KuTaskBlockingJobV1,ticket),offsetof(KuTaskBlockingJobV1,result),
      offsetof(KuTaskBlockingJobV1,exists_value));
#if KU_JSON_DENSE_HAS_COLD_INPUTS
  fprintf(stdout,",\"inputs\":%zu",offsetof(KuTaskBlockingJobV1,inputs));
#else
  fprintf(stdout,",\"path\":%zu,\"content\":%zu",offsetof(KuTaskBlockingJobV1,path),offsetof(KuTaskBlockingJobV1,content));
#endif
#if KU_TASK_BLOCKING_PG
  fprintf(stdout,",\"pg_operation\":%zu,\"pg_pool\":%zu,\"pg_ready\":%zu",
      offsetof(KuTaskBlockingJobV1,pg_operation),offsetof(KuTaskBlockingJobV1,pg_pool),
      offsetof(KuTaskBlockingJobV1,pg_ready));
#endif
#if KU_TASK_BLOCKING_NETWORK
  fprintf(stdout,",\"net_deadline\":%zu,\"net_requested\":%zu,\"net_client\":%zu,\"net_write\":%zu,\"net_owner\":%zu,\"net_addresses\":%zu",
      offsetof(KuTaskBlockingJobV1,net_deadline),offsetof(KuTaskBlockingJobV1,net_requested),
      offsetof(KuTaskBlockingJobV1,net_client),offsetof(KuTaskBlockingJobV1,net_write),
      offsetof(KuTaskBlockingJobV1,net_owner),offsetof(KuTaskBlockingJobV1,net_addresses));
#if !KU_JSON_DENSE_HAS_COLD_INPUTS
  fprintf(stdout,",\"net_config\":%zu,\"net_present\":%zu",offsetof(KuTaskBlockingJobV1,net_config),offsetof(KuTaskBlockingJobV1,net_present));
#endif
#endif
  fputs("},\"input_offsetof\":{",stdout);
#if KU_JSON_DENSE_HAS_COLD_INPUTS
  fprintf(stdout,"\"path\":%zu,\"content\":%zu",offsetof(KuTaskBlockingInputsV1,path),offsetof(KuTaskBlockingInputsV1,content));
#if KU_TASK_BLOCKING_NETWORK
  fprintf(stdout,",\"net_config\":%zu,\"net_present\":%zu",offsetof(KuTaskBlockingInputsV1,net_config),offsetof(KuTaskBlockingInputsV1,net_present));
#endif
#endif
  fprintf(stdout,"},\"fixed_capacity\":2048,\"fixed_bytes\":%zu}",ku_task_blocking_fixed_bytes(2048u));
#else
  fputs("},\"available\":false,\"features\":{},\"offsetof\":{},\"fixed_capacity\":2048,\"fixed_bytes\":null}",stdout);
#endif
}
int main(int argc,char** argv) {
  double cpu_target=0,wall_limit=0;uint64_t repeat=0,request_ms=0,handler_ms=0;
  if(argc!=8 || !ku_json_dense_real(argv[2],&cpu_target) || cpu_target<0 || cpu_target>20
      || !ku_json_dense_unsigned(argv[3],&repeat) || repeat>UINT64_C(50000000)
      || !ku_json_dense_real(argv[4],&wall_limit) || wall_limit<1 || wall_limit>180
      || (strcmp(argv[5],"http") && strcmp(argv[5],"none"))
      || !ku_json_dense_unsigned(argv[6],&request_ms) || !request_ms || request_ms>300000u
      || !ku_json_dense_unsigned(argv[7],&handler_ms) || !handler_ms || handler_ms>300000u
      || (cpu_target>0 && repeat)) {
    fputs("usage: dense FAMILY CPU_SECONDS(0..20) REPEAT(0..50000000) WALL_SECONDS(1..180) CONTEXT(http|none) REQUEST_MS(1..300000) HANDLER_MS(1..300000)\n",stderr);return 2;
  }
  int http_context=!strcmp(argv[5],"http");
  size_t selected[KU_JSON_DENSE_CASE_COUNT],count=0;
  for(size_t i=0;i<KU_JSON_DENSE_CASE_COUNT;i++)
    if(!strcmp(argv[1],ku_json_dense_cases[i].family))selected[count++]=i;
  if(!count){fputs("unknown dense input family\n",stderr);return 2;}
  uint64_t setup_cpu=0,setup_wall=0,begin_cpu=0,begin_wall=0,end_cpu=0,end_wall=0;
  if(!ku_json_dense_clock(CLOCK_THREAD_CPUTIME_ID,&setup_cpu)
      || !ku_json_dense_clock(CLOCK_MONOTONIC,&setup_wall))return 3;
  KuString* golden=(KuString*)calloc(count,sizeof(*golden));
  if(!golden)return 3;
  int status=0;uint64_t correct=0;const char* stop="validation_only";
  for(size_t i=0;i<count;i++) {
    if(!ku_json_dense_call(&ku_json_dense_cases[selected[i]],http_context,request_ms,handler_ms,1,&golden[i])) {
      fprintf(stderr,"dense preflight failed at case %zu\n",selected[i]);status=4;goto cleanup;
    }
  }
#if KU_JSON_DENSE_PHASE_LEDGER
  ku_dense_reset_phases();
#endif
  if(!ku_json_dense_clock(CLOCK_THREAD_CPUTIME_ID,&begin_cpu)
      || !ku_json_dense_clock(CLOCK_MONOTONIC,&begin_wall)) {status=3;goto cleanup;}
  end_cpu=begin_cpu;end_wall=begin_wall;
  if(cpu_target>0 || repeat)for(;;) {
    for(size_t batch=0;batch<KU_JSON_DENSE_BATCH;batch++) {
      if(repeat && correct>=repeat)break;
      size_t at=(size_t)(correct%count);KuString response={0};
      if(!ku_json_dense_call(&ku_json_dense_cases[selected[at]],http_context,request_ms,handler_ms,0,&response)) {
        fprintf(stderr,"dense execution failed at iteration %llu\n",(unsigned long long)correct);status=4;goto cleanup;
      }
#if KU_JSON_DENSE_PHASE_LEDGER
      ku_dense_change_phase(KU_DENSE_VALIDATE);
#endif
      int same=response.len==golden[at].len
          && (!response.len || !memcmp(response.ptr,golden[at].ptr,response.len));
#if KU_JSON_DENSE_PHASE_LEDGER
      ku_dense_change_phase(KU_DENSE_DROP);
#endif
      ku_string_drop(&response);
#if KU_JSON_DENSE_PHASE_LEDGER
      ku_dense_change_phase(KU_DENSE_OFF);
#endif
      if(!same) {fprintf(stderr,"dense output changed at iteration %llu\n",(unsigned long long)correct);status=4;goto cleanup;}
      correct++;
    }
    /* Diagnostic thread-CPU/outer-wall reads are batched, not added to every
     * JSON byte/append. Product handler deadline reads remain unchanged. */
    if(!ku_json_dense_clock(CLOCK_THREAD_CPUTIME_ID,&end_cpu)
        || !ku_json_dense_clock(CLOCK_MONOTONIC,&end_wall)
        || end_cpu<begin_cpu || end_wall<begin_wall) {status=3;goto cleanup;}
    if((double)(end_wall-setup_wall)/1e9>=wall_limit) {stop="wall_limit";status=5;break;}
    if(repeat && correct>=repeat) {stop="iterations";break;}
    if(cpu_target>0 && (double)(end_cpu-begin_cpu)/1e9>=cpu_target) {stop="thread_cpu_budget";break;}
  }
  fprintf(stdout,"{\"schema\":\"ku-json-dense-v1\",\"family\":\"%s\",\"context\":\"%s\",\"iterations\":%llu,\"correct\":%llu,\"thread_cpu_ns\":%llu,\"wall_ns\":%llu,\"setup_thread_cpu_ns\":%llu,\"setup_wall_ns\":%llu,\"batch_size\":%u,\"request_timeout_ms\":%llu,\"handler_timeout_ms\":%llu,\"stop_reason\":\"%s\",\"validation\":[",argv[1],argv[5],
      (unsigned long long)correct,(unsigned long long)correct,
      (unsigned long long)(end_cpu-begin_cpu),(unsigned long long)(end_wall-begin_wall),
      (unsigned long long)(begin_cpu-setup_cpu),(unsigned long long)(begin_wall-setup_wall),
      KU_JSON_DENSE_BATCH,(unsigned long long)request_ms,(unsigned long long)handler_ms,stop);
  for(size_t i=0;i<count;i++) {
    if(i)fputc(',',stdout);
    fprintf(stdout,"{\"case\":%zu,\"body_hex\":\"",selected[i]);ku_json_dense_hex(golden[i]);fputs("\"}",stdout);
  }
  fputc(']',stdout);
  uint64_t cleanup_cpu=0,cleanup_wall=0,done_cpu=0,done_wall=0;
  if(!ku_json_dense_clock(CLOCK_THREAD_CPUTIME_ID,&cleanup_cpu)
      || !ku_json_dense_clock(CLOCK_MONOTONIC,&cleanup_wall)) {status=3;goto cleanup;}
  for(size_t i=0;i<count;i++)ku_string_drop(&golden[i]);free(golden);golden=NULL;
  if(!ku_json_dense_clock(CLOCK_THREAD_CPUTIME_ID,&done_cpu)
      || !ku_json_dense_clock(CLOCK_MONOTONIC,&done_wall)
      || done_cpu<cleanup_cpu || done_wall<cleanup_wall)return 3;
  fprintf(stdout,",\"golden_cleanup_thread_cpu_ns\":%llu,\"golden_cleanup_wall_ns\":%llu,\"layout\":",
      (unsigned long long)(done_cpu-cleanup_cpu),(unsigned long long)(done_wall-cleanup_wall));
  ku_json_dense_layout();
#if KU_JSON_DENSE_PHASE_LEDGER
  ku_dense_write_phases();
#endif
  fputs("}\n",stdout);
  return status;
cleanup:
  for(size_t i=0;i<count;i++)ku_string_drop(&golden[i]);free(golden);return status;
}
