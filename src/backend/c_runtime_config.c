/* Internal service resource policy. The OS stack reservation is a separate
 * cost from Task/HTTP allocation ledgers. Environment parsing never truncates
 * malformed values or silently expands an explicit total thread budget. */
#define KU_RUNTIME_RESOURCE_CONFIG 1
#if defined(_WIN32)
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#include <windows.h>
#else
#include <unistd.h>
#endif
/* Apply each cgroup v2 ancestor: a child reporting "max" does not remove an
 * enclosing finite CPU quota. The kernel membership path is bounded below. */
#if defined(__linux__)
static size_t ku_runtime_cpu_max(const char* path,size_t count) {
  FILE* file=fopen(path,"r");
  if (!file) return count;
  unsigned long long quota=0,period=0;
  if (fscanf(file,"%llu %llu",&quota,&period)==2 && period && quota) {
    unsigned long long limited=quota/period+(quota%period!=0);
    if(limited<count) count=(size_t)limited;
  }
  fclose(file); return count;
}
#endif
static size_t ku_runtime_env_count(const char* name,size_t fallback,size_t maximum) {
  const char* value=getenv(name);
  if (!value) return fallback;
  if (!*value) { fprintf(stderr,"%s must be a positive integer\n",name); exit(1); }
  size_t parsed=0;
  for (const unsigned char* at=(const unsigned char*)value;*at;at++) {
    if (*at<'0' || *at>'9' || parsed>(maximum-(size_t)(*at-'0'))/10u) {
      fprintf(stderr,"%s must be an integer in 1..%zu\n",name,maximum); exit(1);
    }
    parsed=parsed*10u+(size_t)(*at-'0');
  }
  if (!parsed || parsed>maximum) { fprintf(stderr,"%s must be an integer in 1..%zu\n",name,maximum); exit(1); }
  return parsed;
}
static size_t ku_runtime_available_cpu(void) {
  size_t count=1;
#if defined(_WIN32)
  SYSTEM_INFO info; GetSystemInfo(&info);
  if (info.dwNumberOfProcessors) count=(size_t)info.dwNumberOfProcessors;
  DWORD_PTR process_mask=0,system_mask=0;
  if (GetProcessAffinityMask(GetCurrentProcess(),&process_mask,&system_mask) && process_mask) {
    size_t active=0;
    while (process_mask) { active+=(size_t)(process_mask&1u); process_mask>>=1u; }
    if (active<count) count=active;
  }
#if defined(JOB_OBJECT_CPU_RATE_CONTROL_ENABLE) && defined(JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP)
  JOBOBJECT_CPU_RATE_CONTROL_INFORMATION rate={0};
  if (QueryInformationJobObject(NULL,JobObjectCpuRateControlInformation,&rate,sizeof(rate),NULL)
      && (rate.ControlFlags&JOB_OBJECT_CPU_RATE_CONTROL_ENABLE)
      && (rate.ControlFlags&JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP) && rate.CpuRate) {
    size_t limited=(count*(size_t)rate.CpuRate+9999u)/10000u;
    if (limited && limited<count) count=limited;
  }
#endif
#else
  long online=sysconf(_SC_NPROCESSORS_ONLN);
  if (online>0) count=(size_t)online;
#if defined(__linux__)
  FILE* membership=fopen("/proc/self/cgroup","r");
  if (membership) {
    char record[4096];
    while (fgets(record,sizeof(record),membership)) if(strncmp(record,"0::/",4)==0) {
      char* path=record+3; size_t length=strcspn(path,"\r\n"); path[length]=0;
      if(length+sizeof("/sys/fs/cgroup/cpu.max")<4096u) for(;;) {
        char quota_path[4096];
        snprintf(quota_path,sizeof(quota_path),"/sys/fs/cgroup%s/cpu.max",path);
        count=ku_runtime_cpu_max(quota_path,count);
        if(!length || (length==1u && path[0]=='/')) break;
        while(length && path[length-1]!='/') length--;
        if(length>1u) length--;
        path[length]=0;
      }
      break;
    }
    fclose(membership);
  }
  /* Also cover namespace roots and the common cgroup v1 container mount. */
  const char* paths[]={"/sys/fs/cgroup/cpu.max","/sys/fs/cgroup/cpu/cpu.cfs_quota_us"};
  FILE* file=fopen(paths[0],"r");
  if (file) {
    unsigned long long quota=0,period=0;
    if (fscanf(file,"%llu %llu",&quota,&period)==2 && period && quota) {
      unsigned long long limited=quota/period+(quota%period!=0);
      if (limited<count) count=(size_t)limited;
    }
    fclose(file);
  } else {
    file=fopen(paths[1],"r");
    long long quota=-1,period=0;
    if (file) { if (fscanf(file,"%lld",&quota)!=1) quota=-1; fclose(file); }
    file=fopen("/sys/fs/cgroup/cpu/cpu.cfs_period_us","r");
    if (file) { if (fscanf(file,"%lld",&period)!=1) period=0; fclose(file); }
    if (quota>0 && period>0) {
      unsigned long long limited=(unsigned long long)quota/(unsigned long long)period
          +((unsigned long long)quota%(unsigned long long)period!=0);
      if (limited<count) count=(size_t)limited;
    }
  }
  /* Linux's proc status reports the process' actual CPU affinity without
   * requiring GNU CPU_SET declarations before the generated headers. */
  file=fopen("/proc/self/status","r");
  if (file) {
    char line[4096];
    while (fgets(line,sizeof(line),file)) if (strncmp(line,"Cpus_allowed_list:",18)==0) {
      const char* at=line+18; size_t active=0;
      while (*at) {
        if (*at<'0' || *at>'9') { at++; continue; }
        char* end=NULL; unsigned long first=strtoul(at,&end,10); at=end;
        unsigned long last=first;
        if (*at=='-') { at++; last=strtoul(at,&end,10); at=end; }
        if (last>=first && last-first<1048576u && active<=SIZE_MAX-(size_t)(last-first+1u))
          active+=(size_t)(last-first+1u);
        else { active=0; break; }
      }
      if (active && active<count) count=active;
      break;
    }
    fclose(file);
  }
#endif
#endif
  return count ? count : 1u;
}
static size_t ku_runtime_thread_budget(void) {
  size_t budget=ku_runtime_env_count("KU_THREAD_BUDGET",16u,96u);
  if (budget<4u) { fputs("KU_THREAD_BUDGET must include main, output, event and blocking threads\n",stderr); exit(1); }
  return budget;
}
static size_t ku_runtime_loop_count(void) {
  size_t budget=ku_runtime_thread_budget()-2u,available=ku_runtime_available_cpu();
  size_t fallback=available<8u ? available : 8u;
  if (fallback>budget/2u) fallback=budget/2u;
  size_t count=ku_runtime_env_count("KU_EVENT_LOOPS",fallback,32u);
  if (count>=budget) { fputs("KU_EVENT_LOOPS leaves no blocking thread budget\n",stderr); exit(1); }
  return count;
}
static size_t ku_runtime_blocking_count_reserved(size_t reserved) {
  size_t budget=ku_runtime_thread_budget()-2u,loops=ku_runtime_loop_count();
  /* HTTP's background Task worker shares this total with its handler workers.
     Only the implicit default includes that reservation; an explicit count is
     still the caller's exact total and cannot expand the thread budget. */
  size_t fallback=loops+reserved;
  if (fallback<2u) fallback=2u;
  if (fallback>budget-loops) fallback=budget-loops;
  size_t count=ku_runtime_env_count("KU_BLOCKING_THREADS",fallback,64u);
  if (count>budget-loops) { fputs("event and blocking threads exceed KU_THREAD_BUDGET\n",stderr); exit(1); }
  return count;
}
static size_t ku_runtime_blocking_count(void) {
  return ku_runtime_blocking_count_reserved(0u);
}
static size_t ku_runtime_thread_stack_bytes(void) {
  size_t bytes=ku_runtime_env_count("KU_THREAD_STACK_BYTES",2097152u,67108864u);
  if (bytes<262144u || bytes%4096u) {
    fputs("KU_THREAD_STACK_BYTES must be a 4096-byte multiple in 262144..67108864\n",stderr); exit(1);
  }
  return bytes;
}
