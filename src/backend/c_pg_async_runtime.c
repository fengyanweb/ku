/* Resumable pooled libpq query. No poll/condition wait is performed by step.
 * Only connection establishment (libpq may call the system resolver even from
 * PQconnectPoll) uses the pre-existing bounded blocking facility. SQL is sent
 * once on its exclusive lease; cancellation never implies rollback or replay.
 * libpq allocations remain covered by the process limit, not this Ku ledger. */
typedef struct KuPgAsyncBudget {
  void* context;
  int (*reserve)(void*,size_t);
  void (*refund)(void*,size_t);
} KuPgAsyncBudget;
enum { KU_PG_ASYNC_ACQUIRE=1, KU_PG_ASYNC_CONNECT=2,
       KU_PG_ASYNC_PREPARE=3, KU_PG_ASYNC_SEND=4,
       KU_PG_ASYNC_FLUSH=5, KU_PG_ASYNC_READ=6,
       KU_PG_ASYNC_RESET_SEND=7, KU_PG_ASYNC_RESET_FLUSH=8,
       KU_PG_ASYNC_RESET_READ=9, KU_PG_ASYNC_DONE=10 };
typedef struct KuPgAsyncOp {
  KuPgClient* pool;
  KuPgAsyncBudget budget;
  KuPgAsyncWaiter waiter;
  PGconn* conn;
  PGresult* row;
  size_t row_col,row_offset,row_bytes,row_next_rows;
  int row_phase,row_columns,row_rows,row_status;
  KuPgConnectAttempt connected;
  KuPgQuery query;
  KuResult_pg_result output;
  char* sql;
  const char** values;
  char* params;
  size_t sql_bytes,values_bytes,param_bytes,input_charge,operation_charge;
  unsigned long long deadline,acquire_deadline,connect_deadline;
  int phase,slot,count,partial,columns,sent,reset_ok,reset_sent,reset_partial;
  int connecting,connect_ready,waiting,broken,taken;
  unsigned int interest;
} KuPgAsyncOp;
static int ku_pg_result_owned_bytes(KuPgResult* result,size_t* bytes) {
  if (!bytes) return 0;
  *bytes=0; if (!result) return 1;
  if (result->rows && result->cols>SIZE_MAX/result->rows) return 0;
  if (result->rows*result->cols>result->cell_capacity
      || result->bytes_len>result->bytes_capacity
      || (!!result->cell_capacity != !!result->cells)
      || (!!result->bytes_capacity != !!result->bytes)
      || result->rows>KU_PG_MAX_RESULT_ROWS || result->cols>KU_PG_MAX_RESULT_COLS
      || result->cell_capacity>KU_PG_MAX_RESULT_CELLS || result->bytes_capacity>KU_PG_MAX_RESULT_BYTES
      || result->cell_capacity>(SIZE_MAX-sizeof(*result))/sizeof(KuPgCell)) return 0;
  size_t total=sizeof(*result)+result->cell_capacity*sizeof(KuPgCell);
  if (result->bytes_capacity>SIZE_MAX-total) return 0;
  *bytes=total+result->bytes_capacity; return 1;
}
static KuError ku_pg_async_limit_error(void) {
  return ku_pg_static_error("resource_limit",sizeof("resource_limit")-1,
      "PostgreSQL operation exceeds its runtime allocation budget",sizeof("PostgreSQL operation exceeds its runtime allocation budget")-1);
}
static void ku_pg_async_unlink_locked(KuPgAsyncOp* op) {
  if (!op->waiter.linked) return;
  KuPgClient* p=op->pool;
  if (op->waiter.previous) op->waiter.previous->next=op->waiter.next;
  else p->async_head=op->waiter.next;
  if (op->waiter.next) op->waiter.next->previous=op->waiter.previous;
  else p->async_tail=op->waiter.previous;
  op->waiter.next=op->waiter.previous=NULL;op->waiter.linked=0;
  if (op->waiting) { p->waiters--;op->waiting=0; }
}
static void ku_pg_async_drop_table(KuPgAsyncOp* op,KuPgResult** result) {
  size_t bytes=0; (void)ku_pg_result_owned_bytes(*result,&bytes);
  ku_drop_pg_result(result);
  if (bytes) op->budget.refund(op->budget.context,bytes);
}
static void ku_pg_async_drop_query(KuPgAsyncOp* op) {
  if(op->row) {PQclear(op->row);op->row=NULL;}op->row_phase=0;
  if (op->query.terminal) {PQclear(op->query.terminal);op->query.terminal=NULL;}
  ku_pg_async_drop_table(op,&op->query.value);
  ku_error_drop(&op->query.error);op->query=(KuPgQuery){true,0,0,1,{0}};
}
/* Caller holds the only operation executor. A connection is never reused until
 * the terminal NULL and successful DISCARD ALL on the original deadline. */
static void ku_pg_async_release(KuPgAsyncOp* op,int broken) {
  if (op->slot<0) return;
  KuPgClient* p=op->pool; PGconn* discard=NULL;
  ku_pg_mutex_lock(&p->lock);
  if ((broken || p->closing) && p->conns[op->slot]) {
    discard=p->conns[op->slot];p->conns[op->slot]=NULL;
  }
  p->in_use[op->slot]=0;p->active--;op->slot=-1;op->conn=NULL;
  ku_pg_client_signal_locked(p,p->closing);
  ku_pg_mutex_unlock(&p->lock);
  if (discard) PQfinish(discard);
}
static void ku_pg_async_shutdown(KuPgAsyncOp* op) {
  if (!op->conn) return;
  int fd=PQsocket(op->conn);
  if (fd>=0) {
#if defined(_WIN32)
    shutdown((SOCKET)(uintptr_t)(unsigned int)fd,SD_BOTH);
#else
    shutdown(fd,SHUT_RDWR);
#endif
  }
}
static void ku_pg_async_finish(KuPgAsyncOp* op,KuError error,int broken) {
  ku_pg_mutex_lock(&op->pool->lock);ku_pg_async_unlink_locked(op);ku_pg_mutex_unlock(&op->pool->lock);
  ku_pg_async_drop_query(op);
  op->output=(KuResult_pg_result){false,NULL,error};op->broken=broken;
  if (broken) ku_pg_async_shutdown(op);
  ku_pg_async_release(op,broken);op->phase=KU_PG_ASYNC_DONE;op->interest=0;
}
static void ku_pg_async_failure(KuPgAsyncOp* op) {
  int completed=(op->query.terminal && (PQresultStatus(op->query.terminal)==KU_PGRES_COMMAND_OK
      || PQresultStatus(op->query.terminal)==KU_PGRES_TUPLES_OK))
      || (op->row && (op->row_status==KU_PGRES_COMMAND_OK || op->row_status==KU_PGRES_TUPLES_OK));
  ku_pg_async_finish(op,op->sent ? completed ? ku_pg_execution_completed_without_result_error()
      : ku_pg_execution_unknown_error() : ku_pg_query_connection_error(),1);
}
static KuPgAsyncOp* ku_pg_async_create(KuPgClient* p,KuString sql,KuArray_str params,
    unsigned long long absolute_deadline,KuPgAsyncBudget budget,
    void (*wake)(void*),void* context,KuError* error) {
  if (!p || !budget.reserve || !budget.refund || !wake || !error) return NULL;
  unsigned long long now=ku_pg_now_ms();
  unsigned long long deadline=ku_pg_client_saturating_add_ms(now,p->query_timeout_ms);
  if (absolute_deadline && absolute_deadline<deadline) deadline=absolute_deadline;
  if (!ku_pg_validate_sql_input(sql,error,deadline)) return NULL;
  int session=ku_pg_sql_has_explicit_session_control(sql,deadline);
  if (session) { *error=session==-2 ? ku_pg_query_timeout_error()
      : ku_pg_client_error("session_state_unsupported",sizeof("session_state_unsupported")-1,
      "PostgreSQL statement was not sent because explicit transaction or session-control SQL is unsupported by the pooled client",
      sizeof("PostgreSQL statement was not sent because explicit transaction or session-control SQL is unsupported by the pooled client")-1);return NULL; }
  size_t param_bytes=0;
  if (!ku_pg_validate_query_params(params,&param_bytes,error,deadline)) return NULL;
  if (params.len>SIZE_MAX/sizeof(char*) || sql.len>SIZE_MAX-1u
      || param_bytes>SIZE_MAX-params.len) { *error=ku_pg_async_limit_error();return NULL; }
  size_t sql_bytes=sql.len+1u,values_bytes=params.len*sizeof(char*),storage_bytes=param_bytes+params.len;
  if (sql_bytes>SIZE_MAX-sizeof(KuPgAsyncOp) || values_bytes>SIZE_MAX-sizeof(KuPgAsyncOp)-sql_bytes
      || storage_bytes>SIZE_MAX-sizeof(KuPgAsyncOp)-sql_bytes-values_bytes) { *error=ku_pg_async_limit_error();return NULL; }
  size_t charge=sizeof(KuPgAsyncOp)+sql_bytes+values_bytes+storage_bytes,operation_charge=0;
  if (!ku_database_operation_begin(sql.len+param_bytes+params.len*sizeof(KuString),&operation_charge)) {
    *error=ku_pg_async_limit_error();return NULL;
  }
  if (!budget.reserve(budget.context,charge)) {ku_database_operation_end(operation_charge);*error=ku_pg_async_limit_error();return NULL;}
  KuPgAsyncOp* op=(KuPgAsyncOp*)calloc(1,sizeof(*op));
  char* copied_sql=(char*)malloc(sql_bytes);
  const char** values=values_bytes ? (const char**)malloc(values_bytes):NULL;
  char* storage=storage_bytes ? (char*)malloc(storage_bytes):NULL;
  if (!op || !copied_sql || (values_bytes && !values) || (storage_bytes && !storage)) {
    free(op);free(copied_sql);free(values);free(storage);budget.refund(budget.context,charge);
    ku_database_operation_end(operation_charge);*error=ku_pg_out_of_memory_error();return NULL;
  }
  int copied=ku_pg_copy_until(copied_sql,sql.ptr,sql.len,deadline);copied_sql[sql.len]=0;
  char* cursor=storage;
  for(size_t i=0;i<params.len && copied;i++) {values[i]=cursor;copied=ku_pg_copy_until(cursor,params.data[i].ptr,params.data[i].len,deadline);cursor[params.data[i].len]=0;cursor+=params.data[i].len+1u;}
  if(!copied) {free(op);free(copied_sql);free(values);free(storage);budget.refund(budget.context,charge);ku_database_operation_end(operation_charge);*error=ku_pg_query_timeout_error();return NULL;}
  op->pool=p;op->budget=budget;op->sql=copied_sql;op->values=values;op->params=storage;
  op->sql_bytes=sql_bytes;op->values_bytes=values_bytes;op->param_bytes=storage_bytes;
  op->input_charge=charge;op->operation_charge=operation_charge;op->count=(int)params.len;
  op->slot=-1;op->phase=KU_PG_ASYNC_ACQUIRE;op->query=(KuPgQuery){true,0,0,1,{0}};
  op->deadline=deadline;op->acquire_deadline=ku_pg_client_saturating_add_ms(now,p->acquire_timeout_ms);
  if (deadline<op->acquire_deadline) op->acquire_deadline=deadline;
  op->waiter.wake=wake;op->waiter.context=context;
  ku_pg_mutex_lock(&p->lock);
  if(p->closing) {ku_pg_mutex_unlock(&p->lock);free(op);free(copied_sql);free(values);free(storage);budget.refund(budget.context,charge);ku_database_operation_end(operation_charge);
    *error=ku_pg_client_error("client_closed",sizeof("client_closed")-1,"PostgreSQL client is closed",sizeof("PostgreSQL client is closed")-1);return NULL;}
  p->async_operations++;ku_pg_mutex_unlock(&p->lock);return op;
}
/* Return 1 when a background connect is required, 0 while parked/leased,
 * and -1 after a completed acquisition error. Never waits on a condition. */
static int ku_pg_async_acquire(KuPgAsyncOp* op,unsigned long long now) {
  KuPgClient* p=op->pool;KuError error={0};int make=-1;
  ku_pg_mutex_lock(&p->lock);
  if(p->closing) error=ku_pg_client_error("client_closed",sizeof("client_closed")-1,"PostgreSQL client is closed",sizeof("PostgreSQL client is closed")-1);
  else if(now>=op->acquire_deadline) error=ku_pg_client_error("acquire_timeout",sizeof("acquire_timeout")-1,"timed out acquiring a PostgreSQL client connection",sizeof("timed out acquiring a PostgreSQL client connection")-1);
  if(error.code.len) {ku_pg_async_unlink_locked(op);ku_pg_mutex_unlock(&p->lock);ku_pg_async_finish(op,error,0);return -1;}
  int can_claim=op->waiting || p->waiters==0;
  if(can_claim) for(size_t i=0;i<p->size;i++) if(p->conns[i] && !p->in_use[i]) {
    p->in_use[i]=1;p->active++;op->slot=(int)i;op->conn=p->conns[i];ku_pg_async_unlink_locked(op);op->phase=KU_PG_ASYNC_PREPARE;break;
  }
  if(op->slot<0 && can_claim && !p->connect_in_flight && now>=p->reconnect_not_before_ms)
    for(size_t i=0;i<p->size;i++) if(!p->conns[i] && !p->in_use[i]) {make=(int)i;p->in_use[i]=1;p->active++;p->connect_in_flight=1;op->slot=make;op->connecting=1;op->phase=KU_PG_ASYNC_CONNECT;ku_pg_async_unlink_locked(op);break;}
  if(op->slot<0 && !op->waiting) {
    if(p->waiters>=p->max_waiters) error=ku_pg_client_error("pool_busy",sizeof("pool_busy")-1,"PostgreSQL client waiter limit reached",sizeof("PostgreSQL client waiter limit reached")-1);
    else {op->waiting=1;p->waiters++;op->waiter.linked=1;op->waiter.notified=0;
      op->waiter.previous=p->async_tail;if(p->async_tail)p->async_tail->next=&op->waiter;else p->async_head=&op->waiter;p->async_tail=&op->waiter;}
  } else if(op->waiting) op->waiter.notified=0;
  if(make>=0) {op->connect_deadline=ku_pg_client_saturating_add_ms(now,p->connect_timeout_ms);if(op->acquire_deadline<op->connect_deadline)op->connect_deadline=op->acquire_deadline;}
  ku_pg_mutex_unlock(&p->lock);
  if(error.code.len) {ku_pg_async_finish(op,error,0);return -1;}
  return make>=0 ? 1:0;
}
/* Only the bounded worker invokes this function. It touches no source frame.
 * Publication back to NET_WAIT is protected by the enclosing job mutex. */
static void ku_pg_async_connect_run(KuPgAsyncOp* op,int cancelled) {
  if(cancelled) op->connected=(KuPgConnectAttempt){NULL,KU_PG_CONNECT_TIMED_OUT};
  else op->connected=ku_pg_connect_until(op->pool->conninfo,op->connect_deadline);
  op->connect_ready=1;
}
static void ku_pg_async_connected(KuPgAsyncOp* op,unsigned long long now,int cancelled) {
  KuPgClient* p=op->pool;PGconn* connection=op->connected.conn;op->connected.conn=NULL;
  ku_pg_mutex_lock(&p->lock);p->connect_in_flight=0;op->connecting=0;
  int closed=p->closing;
  if(connection && !closed && !cancelled && now<op->acquire_deadline) {
    p->conns[op->slot]=connection;op->conn=connection;ku_pg_client_record_connect_success_locked(p);op->phase=KU_PG_ASYNC_PREPARE;
  } else {if(!connection)ku_pg_client_record_connect_failure_locked(p,now);p->in_use[op->slot]=0;p->active--;op->slot=-1;}
  ku_pg_client_signal_locked(p,closed);ku_pg_mutex_unlock(&p->lock);
  if(op->slot>=0)return;
  if(connection)PQfinish(connection);
  KuError error=closed ? ku_pg_client_error("client_closed",sizeof("client_closed")-1,"PostgreSQL client is closed",sizeof("PostgreSQL client is closed")-1)
      : cancelled ? ku_pg_client_error("cancelled",sizeof("cancelled")-1,"PostgreSQL query was cancelled before it was sent",sizeof("PostgreSQL query was cancelled before it was sent")-1)
      : op->connected.outcome==KU_PG_CONNECT_OUT_OF_MEMORY ? ku_pg_out_of_memory_error()
      : now>=op->acquire_deadline ? ku_pg_client_error("acquire_timeout",sizeof("acquire_timeout")-1,"timed out acquiring a PostgreSQL client connection",sizeof("timed out acquiring a PostgreSQL client connection")-1)
      : op->connected.outcome==KU_PG_CONNECT_TIMED_OUT ? ku_pg_client_error("connect_timeout",sizeof("connect_timeout")-1,"timed out connecting a PostgreSQL client connection",sizeof("timed out connecting a PostgreSQL client connection")-1)
      : ku_pg_connect_failure_error();
  ku_pg_async_finish(op,error,0);
}
/* At most 64 KiB of UTF8 validation/copy and 64 cells per owner turn.
 * The current libpq single-row PGresult is retained until its bytes have been
 * copied; Ku's detached table never refers into it after PQclear. */
static int ku_pg_async_materialize(KuPgAsyncOp* op) {
  const size_t byte_quantum=65536u,cell_quantum=64u;
  size_t work=0,cells_work=0;
  if(!op->row_phase) {
    int rows=PQntuples(op->row),cols=PQnfields(op->row);
    size_t previous=op->query.value ? op->query.value->rows:0;
    if(rows<0 || rows>1 || cols<0 || (size_t)cols>KU_PG_MAX_RESULT_COLS
        || previous>KU_PG_MAX_RESULT_ROWS-(size_t)rows
        || ((previous+(size_t)rows) && (size_t)cols>KU_PG_MAX_RESULT_CELLS/(previous+(size_t)rows))
        || (op->query.value && op->query.value->cols!=(size_t)cols))return -1;
    op->row_rows=rows;op->row_columns=cols;op->row_next_rows=previous+(size_t)rows;
    op->row_bytes=op->query.value ? op->query.value->bytes_len:0;
    op->row_phase=1;op->row_col=op->row_offset=0;
  }
  if(op->row_phase==1) {
    while(op->row_col<(size_t)op->row_columns) {
      int col=(int)op->row_col;
      if(PQfformat(op->row,col)!=0)return -4;
      if(!op->row_rows || PQgetisnull(op->row,0,col)) {op->row_col++;if(++cells_work>=cell_quantum)return 0;continue;}
      int length=PQgetlength(op->row,0,col);
      if(length<0 || (size_t)length>KU_PG_MAX_RESULT_BYTES-op->row_bytes)return -1;
      size_t remaining=(size_t)length-op->row_offset;
      size_t count=remaining<byte_quantum-work?remaining:byte_quantum-work;
      const uint8_t* source=(const uint8_t*)PQgetvalue(op->row,0,col);
      if(count<remaining) {
        size_t end=op->row_offset+count;
        while(end>op->row_offset && (source[end]&0xc0u)==0x80u)end--;
        count=end-op->row_offset;
        if(!count)return 0;
      }
      if(count) {
        int valid=ku_pg_utf8_valid(source+op->row_offset,count,op->deadline);
        if(valid!=1)return valid<0?-2: -5;
        op->row_offset+=count;work+=count;
      }
      if(op->row_offset==(size_t)length) {op->row_bytes+=(size_t)length;op->row_col++;op->row_offset=0;cells_work++;}
      if(work>=byte_quantum || cells_work>=cell_quantum)return 0;
    }
    op->row_phase=2;
  }
  if(op->row_phase==2) {
    size_t previous=0;if(!ku_pg_result_owned_bytes(op->query.value,&previous))return -1;
    size_t cells=op->row_next_rows*(size_t)op->row_columns;
    size_t cc=op->query.value?op->query.value->cell_capacity:0,bc=op->query.value?op->query.value->bytes_capacity:0;
    if(cells>cc)cc=ku_pg_result_capacity(cc,cells,16,(size_t)KU_PG_MAX_RESULT_CELLS);
    if(op->row_bytes>bc)bc=ku_pg_result_capacity(bc,op->row_bytes,256,(size_t)KU_PG_MAX_RESULT_BYTES);
    if(cc>(SIZE_MAX-sizeof(KuPgResult))/sizeof(KuPgCell) || bc>SIZE_MAX-sizeof(KuPgResult)-cc*sizeof(KuPgCell))return -1;
    size_t predicted=sizeof(KuPgResult)+cc*sizeof(KuPgCell)+bc;
    if(predicted<previous || !op->budget.reserve(op->budget.context,predicted-previous))return -3;
    KuPgResult* result=op->query.value;
    if(!result) {result=(KuPgResult*)calloc(1,sizeof(*result));op->query.value=result;if(result)result->cols=(size_t)op->row_columns;}
    int allocated=result!=NULL;
    if(allocated && cc>result->cell_capacity) {KuPgCell* grown=(KuPgCell*)realloc(result->cells,cc*sizeof(KuPgCell));if(!grown)allocated=0;else {result->cells=grown;result->cell_capacity=cc;}}
    if(allocated && bc>result->bytes_capacity) {uint8_t* grown=(uint8_t*)realloc(result->bytes,bc);if(!grown)allocated=0;else {result->bytes=grown;result->bytes_capacity=bc;}}
    size_t actual=0;if(!ku_pg_result_owned_bytes(result,&actual) || actual>predicted)return -3;
    if(predicted>actual)op->budget.refund(op->budget.context,predicted-actual);
    if(!allocated)return -3;
    op->row_phase=3;op->row_col=op->row_offset=0;
    /* Allocation/growth and copy use separate bounded owner turns. */
    return 0;
  }
  while(op->row_rows && op->row_col<(size_t)op->row_columns) {
    KuPgResult* result=op->query.value;int col=(int)op->row_col;
    KuPgCell* cell=&result->cells[result->rows*result->cols+op->row_col];
    if(!op->row_offset) {
      cell->offset=(uint32_t)result->bytes_len;
      cell->len=PQgetisnull(op->row,0,col)?KU_PG_NULL_CELL:(uint32_t)PQgetlength(op->row,0,col);
    }
    if(cell->len==KU_PG_NULL_CELL || !cell->len) {op->row_col++;if(++cells_work>=cell_quantum)return 0;continue;}
    size_t remaining=(size_t)cell->len-op->row_offset;
    size_t count=remaining<byte_quantum-work?remaining:byte_quantum-work;
    if(count)memcpy(result->bytes+result->bytes_len,(const uint8_t*)PQgetvalue(op->row,0,col)+op->row_offset,count);
    result->bytes_len+=count;op->row_offset+=count;work+=count;
    if(op->row_offset==(size_t)cell->len) {op->row_col++;op->row_offset=0;cells_work++;}
    if(work>=byte_quantum || cells_work>=cell_quantum)return 0;
  }
  op->query.value->rows=op->row_next_rows;return 1;
}
static int ku_pg_async_step(KuPgAsyncOp* op,unsigned int events,unsigned long long now,int cancelled) {
  if(op->phase==KU_PG_ASYNC_DONE)return 1;
  if(op->phase==KU_PG_ASYNC_CONNECT) {
    if(!op->connect_ready)return 0;
    ku_pg_async_connected(op,now,cancelled);if(op->phase==KU_PG_ASYNC_DONE)return 1;
  }
  if(cancelled || now>=op->deadline) {
    /* A fully detached user result remains valid even if its mandatory reset
     * times out. Evict the lease; never send its statement again. */
    if(op->phase>=KU_PG_ASYNC_RESET_SEND) {ku_pg_async_shutdown(op);ku_pg_async_release(op,1);op->phase=KU_PG_ASYNC_DONE;return 1;}
    ku_pg_async_finish(op,op->sent ? ku_pg_execution_unknown_error() : cancelled
        ? ku_pg_client_error("cancelled",sizeof("cancelled")-1,"PostgreSQL query was cancelled before it was sent",sizeof("PostgreSQL query was cancelled before it was sent")-1)
        : ku_pg_query_timeout_error(),1);return 1;
  }
  if(op->phase==KU_PG_ASYNC_ACQUIRE) {int acquired=ku_pg_async_acquire(op,now);if(acquired || op->phase==KU_PG_ASYNC_ACQUIRE)return op->phase==KU_PG_ASYNC_DONE;}
  /* A maximum of 32 protocol results per turn. Byte/row hard limits and the
   * original absolute deadline also apply in the shared detach implementation. */
  for(size_t work=0;work<32u;work++) {
    if(ku_pg_deadline_expired(op->deadline)) {
      if(op->phase>=KU_PG_ASYNC_RESET_SEND)goto reset_failed;
      if(op->sent)ku_pg_async_failure(op);else ku_pg_async_finish(op,ku_pg_query_timeout_error(),1);
      return 1;
    }
    op->interest=0;
    if(op->row) {
      int appended=ku_pg_async_materialize(op);
      if(!appended)return 0;
      if(appended==-2 || appended==-3) {ku_pg_async_failure(op);return 1;}
      if(appended!=1) {ku_pg_async_drop_table(op,&op->query.value);op->query.validation=appended;}
      if(op->row_status==KU_PGRES_SINGLE_TUPLE) {op->partial=1;PQclear(op->row);}
      else {op->partial=0;op->query.terminal=op->row;}
      op->row=NULL;op->row_phase=0;
      if(op->row_status==KU_PGRES_COMMAND_OK)op->partial=0;
      continue;
    }
    if(op->phase==KU_PG_ASYNC_PREPARE) {
      int tx=PQtransactionStatus(op->conn);
      if(PQstatus(op->conn)!=KU_PG_CONNECTION_OK || tx!=KU_PQTRANS_IDLE || PQsetnonblocking(op->conn,1)!=0
          || !PQconsumeInput(op->conn) || !ku_pg_connection_is_utf8(op->conn)) {ku_pg_async_failure(op);return 1;}
      op->phase=KU_PG_ASYNC_SEND;
    }
    if(op->phase==KU_PG_ASYNC_SEND) {
      /* Set sent before entering libpq: a failed send can already have placed
       * bytes on the socket. It is never evidence that no side effect occurred. */
      op->sent=1;
      if(!PQsendQueryParams(op->conn,op->sql,op->count,NULL,op->values,NULL,NULL,0)
          || !PQsetSingleRowMode(op->conn)) {ku_pg_async_failure(op);return 1;}
      op->phase=KU_PG_ASYNC_FLUSH;
    }
    if(op->phase==KU_PG_ASYNC_RESET_SEND) {
      if(PQstatus(op->conn)!=KU_PG_CONNECTION_OK || PQtransactionStatus(op->conn)!=KU_PQTRANS_IDLE || !ku_pg_connection_is_utf8(op->conn)
          || !PQsendQueryParams(op->conn,"DISCARD ALL",0,NULL,NULL,NULL,NULL,0) || !PQsetSingleRowMode(op->conn))goto reset_failed;
      op->reset_sent=1;op->phase=KU_PG_ASYNC_RESET_FLUSH;
    }
    int reset=op->phase>=KU_PG_ASYNC_RESET_FLUSH;
    if(op->phase==KU_PG_ASYNC_FLUSH || op->phase==KU_PG_ASYNC_RESET_FLUSH) {
      int flushed=PQflush(op->conn);
      if(flushed<0) {if(reset)goto reset_failed;ku_pg_async_failure(op);return 1;}
      if(flushed) {
        if((events&1u) && !PQconsumeInput(op->conn)) {if(reset)goto reset_failed;ku_pg_async_failure(op);return 1;}
        op->interest=3u;return 0;
      }
      op->phase=reset ? KU_PG_ASYNC_RESET_READ:KU_PG_ASYNC_READ;
    }
    if(PQisBusy(op->conn)) {
      if(!PQconsumeInput(op->conn)) {if(reset)goto reset_failed;ku_pg_async_failure(op);return 1;}
      if(PQisBusy(op->conn)) {op->interest=1u;return 0;}
    }
    PGresult* next=PQgetResult(op->conn);
    if(reset) {
      if(!next) {
        if(!op->reset_ok || PQstatus(op->conn)!=KU_PG_CONNECTION_OK || PQtransactionStatus(op->conn)!=KU_PQTRANS_IDLE || !ku_pg_connection_is_utf8(op->conn))goto reset_failed;
        ku_pg_async_release(op,0);op->phase=KU_PG_ASYNC_DONE;return 1;
      }
      int status=PQresultStatus(next);op->reset_ok=status==KU_PGRES_COMMAND_OK;PQclear(next);
      if(!op->reset_ok)goto reset_failed;
      continue;
    }
    if(!next) {
      if(op->partial || !op->query.terminal || PQstatus(op->conn)!=KU_PG_CONNECTION_OK) {ku_pg_async_failure(op);return 1;}
      int status=PQresultStatus(op->query.terminal);
      if(status!=KU_PGRES_COMMAND_OK && status!=KU_PGRES_TUPLES_OK) {
        op->output=(KuResult_pg_result){false,NULL,ku_pg_static_error("query_error",sizeof("query_error")-1,"PostgreSQL query failed",sizeof("PostgreSQL query failed")-1)};
      } else if(op->query.validation!=1 || !op->query.value) {
        op->output=(KuResult_pg_result){false,NULL,ku_pg_execution_completed_without_result_error()};
      } else if(PQtransactionStatus(op->conn)!=KU_PQTRANS_IDLE) {
        op->output=(KuResult_pg_result){false,NULL,ku_pg_post_execution_session_state_error()};
      } else {op->output=(KuResult_pg_result){true,op->query.value,{0}};op->query.value=NULL;}
      ku_pg_async_drop_query(op);
      if(PQtransactionStatus(op->conn)!=KU_PQTRANS_IDLE || !ku_pg_connection_is_utf8(op->conn))goto reset_failed;
      op->phase=KU_PG_ASYNC_RESET_SEND;continue;
    }
    int status=PQresultStatus(next);
    if(status==KU_PGRES_SINGLE_TUPLE || status==KU_PGRES_TUPLES_OK) {
      int rows=PQntuples(next),columns=PQnfields(next);
      if(rows!=(status==KU_PGRES_SINGLE_TUPLE?1:0) || columns<0 || (op->partial && op->columns!=columns)) {PQclear(next);ku_pg_async_failure(op);return 1;}
      if(!op->partial) {ku_pg_async_drop_query(op);op->columns=columns;}
      if(op->query.validation==1) {
        op->row=next;op->row_phase=0;op->row_status=status;return 0;
      }
      if(status==KU_PGRES_SINGLE_TUPLE) {op->partial=1;PQclear(next);}
      else {op->partial=0;op->query.terminal=next;}
    } else {
      if(status==KU_PGRES_COPY_IN || status==KU_PGRES_COPY_OUT || status==KU_PGRES_COPY_BOTH || status==KU_PGRES_BAD_RESPONSE
          || status<KU_PGRES_EMPTY_QUERY || status>KU_PGRES_SINGLE_TUPLE) {PQclear(next);ku_pg_async_failure(op);return 1;}
      ku_pg_async_drop_query(op);op->query.terminal=next;op->partial=0;
      if(status==KU_PGRES_COMMAND_OK) {
        op->query.terminal=NULL;op->row=next;op->row_phase=0;op->row_status=status;return 0;
      }
    }
    now=ku_pg_now_ms();
    if(now>=op->deadline) {ku_pg_async_failure(op);return 1;}
  }
  /* Buffered protocol data needs another bounded turn, not fresh socket I/O. */
  op->interest=0;return 0;
reset_failed:
  ku_pg_async_shutdown(op);ku_pg_async_release(op,1);op->phase=KU_PG_ASYNC_DONE;op->interest=0;return 1;
}
static int ku_pg_async_interest(KuPgAsyncOp* op,int* fd,unsigned int* events,unsigned long long* deadline) {
  *fd=-1;*events=0;*deadline=op->deadline;
  if(op->phase==KU_PG_ASYNC_ACQUIRE) {
    ku_pg_mutex_lock(&op->pool->lock);
    unsigned long long backoff=op->pool->reconnect_not_before_ms;
    int notified=op->waiter.notified,closed=op->pool->closing;
    ku_pg_mutex_unlock(&op->pool->lock);
    *deadline=op->acquire_deadline;
    if(backoff>ku_pg_now_ms() && backoff<*deadline)*deadline=backoff;
    return notified || closed;
  }
  if(op->phase==KU_PG_ASYNC_CONNECT)return op->connect_ready;
  if(op->phase==KU_PG_ASYNC_DONE)return 1;
  if(!op->interest)return 1;
  *fd=PQsocket(op->conn);*events=op->interest;return *fd<0;
}
static int ku_pg_async_can_dispose(KuPgAsyncOp* op) {
  if(op->phase!=KU_PG_ASYNC_DONE || op->connecting)return 0;
  ku_pg_mutex_lock(&op->pool->lock);ku_pg_async_unlink_locked(op);
  int ready=op->waiter.in_flight==0;ku_pg_mutex_unlock(&op->pool->lock);return ready;
}
static KuResult_pg_result ku_pg_async_take(KuPgAsyncOp* op) {
  if(op->taken || !ku_pg_async_can_dispose(op))return (KuResult_pg_result){false,NULL,ku_pg_query_connection_error()};
  op->taken=1;KuResult_pg_result result=op->output;op->output=(KuResult_pg_result){0};return result;
}
static void ku_pg_async_dispose(KuPgAsyncOp* op,int transfer_result) {
  if(!op || !ku_pg_async_can_dispose(op))return;
  KuPgClient* p=op->pool;KuPgAsyncBudget budget=op->budget;
  if(transfer_result && op->taken) { /* The bridge reconciles the transferred table in its frame receipt. */ }
  else if(op->output.ok)ku_pg_async_drop_table(op,&op->output.value);
  else ku_error_drop(&op->output.error);
  ku_pg_async_drop_query(op);
  free(op->sql);free(op->values);free(op->params);
  size_t charge=op->input_charge,operation_charge=op->operation_charge;
  free(op);budget.refund(budget.context,charge);ku_database_operation_end(operation_charge);
  ku_pg_mutex_lock(&p->lock);p->async_operations--;int dispose=ku_pg_client_take_dispose_locked(p);ku_pg_mutex_unlock(&p->lock);
  if(dispose)ku_pg_client_dispose(p);
}
