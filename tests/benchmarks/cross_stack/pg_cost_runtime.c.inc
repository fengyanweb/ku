/* DIAGNOSTIC ONLY. Uses the existing bounded per-thread flow ledger. */
#include <time.h>
#include <errno.h>
static _Thread_local void* ku_pg_cost_active;
static _Thread_local unsigned int ku_pg_cost_phase;
static _Thread_local unsigned long long ku_pg_cost_cpu,ku_pg_cost_wall;
static _Thread_local unsigned long long ku_pg_cost_step_cpu,ku_pg_cost_step_wall,ku_pg_cost_step_calls,ku_pg_cost_op_sequence;
static _Thread_local int ku_pg_cost_sampling,ku_pg_cost_dispatch_sampling;
static unsigned long long ku_pg_cost_clock(int cpu) {
  int saved=errno;struct timespec value={0};
  int status=clock_gettime(cpu?CLOCK_THREAD_CPUTIME_ID:CLOCK_MONOTONIC,&value);
  if(status)ku_flow_count(KUF_PG_COST_CLOCK_FAILURE);
  errno=saved;
  return status?0:(unsigned long long)value.tv_sec*1000000000ULL+(unsigned long long)value.tv_nsec;
}
static unsigned long long ku_pg_cost_elapsed(unsigned long long start,unsigned long long end) {
  if(!start || !end)return 0;
  if(end<start) {ku_flow_count(KUF_PG_COST_CLOCK_REGRESSED);return 0;}
  return end-start;
}
static unsigned long long ku_pg_cost_difference(unsigned long long total,unsigned long long nested) {
  if(total<nested) {ku_flow_count(KUF_PG_COST_CLOCK_REGRESSED);return 0;}
  return total-nested;
}
static unsigned int ku_pg_cost_base(unsigned int phase) {
  if(phase>10u)phase=0u;
  return KUF_PG_COST_UNKNOWN_CPU_NS+phase*13u;
}
static void ku_pg_cost_suspend(unsigned int phase,unsigned long long wall) {
  ku_flow_add(ku_pg_cost_base(phase)+2u,wall);
}
static void ku_pg_cost_transition(void* operation,unsigned int next) {
  if(ku_pg_cost_active!=operation)return;
  if(!ku_pg_cost_sampling) {ku_pg_cost_phase=next;return;}
  unsigned long long cpu=ku_pg_cost_clock(1),wall=ku_pg_cost_clock(0);
  unsigned int base=ku_pg_cost_base(ku_pg_cost_phase);
  ku_flow_add(base,ku_pg_cost_elapsed(ku_pg_cost_cpu,cpu));
  ku_flow_add(base+1u,ku_pg_cost_elapsed(ku_pg_cost_wall,wall));
  ku_flow_count(base+3u);
  ku_pg_cost_phase=next;ku_pg_cost_cpu=cpu;ku_pg_cost_wall=wall;
}
static void ku_pg_cost_call(unsigned int call) {
  if(ku_pg_cost_active)ku_flow_count(ku_pg_cost_base(ku_pg_cost_phase)+4u+call);
}
/* A function-like macro evaluates its arguments exactly once in the real call.
 * The disabled self-expansion rule leaves the inner libpq name unmodified. */
#define PQsendQueryParams(...) (ku_pg_cost_call(0u),PQsendQueryParams(__VA_ARGS__))
#define PQflush(...) (ku_pg_cost_call(1u),PQflush(__VA_ARGS__))
#define PQconsumeInput(...) (ku_pg_cost_call(2u),PQconsumeInput(__VA_ARGS__))
#define PQisBusy(...) (ku_pg_cost_call(3u),PQisBusy(__VA_ARGS__))
#define PQgetResult(...) (ku_pg_cost_call(4u),PQgetResult(__VA_ARGS__))
#define PQsetSingleRowMode(...) (ku_pg_cost_call(5u),PQsetSingleRowMode(__VA_ARGS__))
#define PQtransactionStatus(...) (ku_pg_cost_call(6u),PQtransactionStatus(__VA_ARGS__))
#define PQstatus(...) (ku_pg_cost_call(7u),PQstatus(__VA_ARGS__))
#define PQparameterStatus(...) (ku_pg_cost_call(8u),PQparameterStatus(__VA_ARGS__))
