/* PG jobs use the same bounded physical slots and generation checks as TCP.
 * Only connection establishment enters the worker ring; SQL/reset are stepped
 * by the immutable Task owner. A stable job stays registered through all pool
 * notification pins, even after its result has become logically complete. */
static int ku_task_pg_reserve(void* raw,size_t bytes) {
  KuTaskBlockingJobV1* job=(KuTaskBlockingJobV1*)raw;
  KuTaskBlockingOutputV1 output={job->pg_pool,job,{NULL,NULL},{0,0},KU_TASK_DRIVER_OK,NULL,0,0,0};
  return ku_task_blocking_output_reserve(&output,bytes)==KU_TASK_DRIVER_OK;
}
static void ku_task_pg_refund(void* raw,size_t bytes) {
  KuTaskBlockingJobV1* job=(KuTaskBlockingJobV1*)raw;
  KuTaskBlockingOutputV1 output={job->pg_pool,job,{NULL,NULL},{0,0},KU_TASK_DRIVER_OK,NULL,0,0,0};
  ku_task_blocking_output_refund(&output,bytes);
}
static void ku_task_pg_wake(void* raw) {
  KuTaskBlockingJobV1* job=(KuTaskBlockingJobV1*)raw;
  KuTaskBlockingPoolV1* pool=job->pg_pool;
  if(ku_task_blocking_lock(pool))return;
  /* The PG pool pinned the operation before dropping its mutex. Disposal and
   * generation reuse cannot occur until this callback has returned. */
  job->pg_ready=1;
  if(ku_task_driver_wake(&job->ticket)!=KU_TASK_DRIVER_OK)pool->fault=KU_TASK_DRIVER_INTERNAL;
  ku_task_blocking_unlock(pool);
}
static void ku_task_pg_connect_run(KuTaskBlockingJobV1* job,int cancelled) {
  ku_pg_async_connect_run(job->pg_operation,cancelled);
}
/* The operation's physical registration retains this control until retirement.
 * Failure capture may read its atomic phase without taking a driver/job lock. */
static uint64_t ku_task_pg_diagnostic_phase(void* raw) {
  return (uint64_t)ku_task_control_atomic_load(&((KuTaskControlV1*)raw)->phase);
}
static uint32_t ku_task_blocking_take_pg(const KuTaskDriverTicketV1* ticket,
    KuTaskBlockingTokenV1* token,KuResult_pg_result* output) {
  return ku_task_blocking_take(ticket,token,0,9u,NULL,output);
}
static uint32_t ku_task_blocking_submit_pg(const KuTaskDriverTicketV1* ticket,
    KuTaskControlV1* control,KuTaskBlockingTokenV1* token,
    KuPgClient* client,KuString sql,KuArray_str params,uint64_t deadline,
    void* context,uint32_t (*reconcile)(void*),KuResult_pg_result* output) {
  if(!ticket || !ticket->driver || !control || !token || token->pool || token->charged_bytes
      || !context || !reconcile || !output)return KU_TASK_DRIVER_INVALID_ARGUMENT;
  if(!client) {*output=(KuResult_pg_result){false,NULL,ku_pg_client_error("client_closed",sizeof("client_closed")-1,
      "PostgreSQL client is closed",sizeof("PostgreSQL client is closed")-1)};return KU_TASK_DRIVER_OK;}
  KuTaskBlockingPoolV1* pool=(KuTaskBlockingPoolV1*)ticket->driver->blocking_pool;
  if(!pool || pool->initialized!=KU_TASK_DRIVER_STORAGE_LIVE) {
    *output=(KuResult_pg_result){false,NULL,ku_task_blocking_error("blocking_pool_stopped","blocking pool is not available")};return KU_TASK_DRIVER_OK;
  }
  if(ku_task_blocking_lock(pool))return KU_TASK_DRIVER_INTERNAL;
  uint32_t status=pool->closing || pool->fault || pool->workers_created!=pool->worker_target
      ? KU_TASK_DRIVER_CLOSED : pool->resident>=pool->capacity ? KU_TASK_DRIVER_LIMIT:KU_TASK_DRIVER_OK;
  size_t index=0;
  if(status==KU_TASK_DRIVER_OK) {
    for(;index<pool->capacity;index++)if(pool->jobs[index].state==KU_TASK_BLOCKING_FREE && pool->jobs[index].generation!=UINT64_MAX)break;
    if(index==pool->capacity)status=KU_TASK_DRIVER_LIMIT;
  }
  if(status!=KU_TASK_DRIVER_OK) {
    if(pool->rejected!=UINT64_MAX)pool->rejected++;
    if(status==KU_TASK_DRIVER_LIMIT && pool->rejected_queue!=UINT64_MAX)pool->rejected_queue++;
    ku_task_blocking_unlock(pool);*output=(KuResult_pg_result){false,NULL,
      ku_task_blocking_error(status==KU_TASK_DRIVER_LIMIT?"blocking_queue_full":"blocking_pool_stopped",
      status==KU_TASK_DRIVER_LIMIT?"blocking queue is full":"blocking pool is closing")};return KU_TASK_DRIVER_OK;
  }
  KuTaskBlockingJobV1* job=&pool->jobs[index];job->generation++;job->state=KU_TASK_BLOCKING_BUILDING;
  job->kind=9u;job->ticket=*ticket;job->pg_pool=pool;
  *token=(KuTaskBlockingTokenV1){pool,index,0,job->generation};
  pool->resident++;if(pool->resident>pool->peak_resident)pool->peak_resident=pool->resident;
  ku_task_blocking_unlock(pool);
  status=ku_task_driver_blocking_register(ticket);int registered=status==KU_TASK_DRIVER_OK;
  KuError error={0};KuPgAsyncOp* operation=NULL;
  if(registered)status=reconcile(context);
  if(status==KU_TASK_DRIVER_OK) {
    operation=ku_pg_async_create(client,sql,params,deadline,(KuPgAsyncBudget){job,ku_task_pg_reserve,ku_task_pg_refund},ku_task_pg_wake,job,&error);
    if(!operation && !error.code.len)error=ku_pg_async_limit_error();
  }
  if(operation) {
    if(ku_task_blocking_lock(pool))return KU_TASK_DRIVER_INTERNAL;
    job->pg_operation=operation;
    if(ku_task_driver_lock(ticket->driver)) {ku_task_blocking_unlock(pool);return KU_TASK_DRIVER_INTERNAL;}
    KuTaskDriverSlotV1* owner=ku_task_driver_find(ticket);
    if(owner)job->net_owner=owner->owner_worker;
    KuPgDiagnosticLease* diagnostic=&operation->waiter.diagnostic;
    diagnostic->task_slot=ticket->slot;diagnostic->task_generation=ticket->generation;
    diagnostic->http_task_slot=ticket->slot;diagnostic->http_task_generation=ticket->generation;
    diagnostic->owner=job->net_owner;diagnostic->job_generation=job->generation;
    diagnostic->parent_chain_missing=owner==NULL;
    /* Existing wait edges carry IDs, not references. Sample at most eight
     * coherent edges while already holding the driver lock, before the PG
     * operation becomes visible to another owner. HTTP joins the candidate
     * identity against its own generation-checked failure sample; a detached
     * child with no HTTP ancestor simply has no matching request record. */
    KuTaskDriverSlotV1* ancestor=owner;size_t child_slot=ticket->slot;
    for(size_t depth=0;ancestor && ancestor->waiter_parent_generation;depth++) {
      if(depth==8u || ancestor->waiter_parent_slot>=ticket->driver->capacity) {diagnostic->parent_chain_missing=1;break;}
      size_t parent_slot=ancestor->waiter_parent_slot;
      KuTaskDriverSlotV1* parent=&ticket->driver->slots[parent_slot];
      if(parent->generation!=ancestor->waiter_parent_generation || parent->wait_child_slot!=child_slot
          || parent->wait_child_generation!=ancestor->generation) {diagnostic->parent_chain_missing=1;break;}
      diagnostic->http_task_slot=parent_slot;diagnostic->http_task_generation=parent->generation;
      diagnostic->parent_depth++;child_slot=parent_slot;ancestor=parent;
    }
    operation->task_phase_context=control;operation->task_phase=ku_task_pg_diagnostic_phase;
    int attached=job->net_owner && (ticket->driver->external_loops || ticket->driver->io_wait);
    ku_task_driver_unlock(ticket->driver);
    if(attached) {
      job->pg_ready=1;job->net_deadline=operation->deadline;
      status=ku_task_blocking_net_state_locked(pool,job,KU_TASK_BLOCKING_NET_WAIT);
      if(pool->closing || ku_task_control_atomic_load(&control->phase)!=KU_TASK_CONTROL_LIVE)job->cancelled=1;
      if(pool->accepted!=UINT64_MAX)pool->accepted++;
      (void)ku_task_driver_wake(ticket);ku_task_blocking_unlock(pool);
      status=ku_task_driver_set_intent(ticket,KU_TASK_DRIVER_WAIT);
      return status==KU_TASK_DRIVER_OK?KU_TASK_DRIVER_PENDING:status;
    }
    ku_task_blocking_unlock(pool);
    ku_pg_async_finish(operation,ku_task_blocking_error("reactor_unavailable","PostgreSQL query requires an attached owner reactor"),0);
    ku_pg_async_dispose(operation,0);operation=NULL;
    error=ku_task_blocking_error("reactor_unavailable","PostgreSQL query requires an attached owner reactor");
  }
  if(ku_task_blocking_lock(pool))return KU_TASK_DRIVER_INTERNAL;
  uint64_t generation=job->generation;memset(job,0,sizeof(*job));job->generation=generation;pool->resident--;
  if(pool->rejected!=UINT64_MAX)pool->rejected++;
  ku_task_blocking_signal(pool);ku_task_blocking_unlock(pool);
  if(registered && ku_task_driver_blocking_retired(ticket)!=KU_TASK_DRIVER_OK)return KU_TASK_DRIVER_INTERNAL;
  *token=(KuTaskBlockingTokenV1){0};uint32_t reconciled=reconcile(context);
  if(reconciled!=KU_TASK_DRIVER_OK)return reconciled;
  if(!error.code.len)error=ku_task_blocking_error("memory_limit","PostgreSQL operation could not be admitted");
  *output=(KuResult_pg_result){false,NULL,error};return KU_TASK_DRIVER_OK;
}
