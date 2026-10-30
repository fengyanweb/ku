/* Explicit binary single-row PG values. No general KuValue clone/drop changes.
 * The high bit tags a private typed owner; all physical-size users mask it. */
#include <limits.h>
typedef char KuPgTypedCellAlignment[(sizeof(uint32_t)%_Alignof(KuPgCell)==0)?1:-1];
#define KU_PG_TYPED_NULLABLE UINT32_C(0x80000000)
#define KU_PG_TYPED_OID_MASK UINT32_C(0x7fffffff)
enum { KU_PG_TYPED_SCHEMA=-10, KU_PG_TYPED_OID=-11, KU_PG_TYPED_FORMAT=-12,
       KU_PG_TYPED_LENGTH=-13, KU_PG_TYPED_NULL=-14, KU_PG_TYPED_ROWS=-15,
       KU_PG_TYPED_VALUE=-16 };
typedef struct KuPgTypedQuery { size_t parameters,columns,rows_seen; } KuPgTypedQuery;
static KuError ku_pg_value_error(void);
static KuError ku_pg_typed_error(int reason) {
  const char* code=reason==KU_PG_TYPED_ROWS?"row_count_error":reason==KU_PG_TYPED_NULL?"null_value":"typed_value_error";
  const char* message=reason==KU_PG_TYPED_ROWS?"PostgreSQL typed query requires exactly one row; the statement is not retried"
      :reason==KU_PG_TYPED_NULL?"PostgreSQL NULL requires a nullable type declaration"
      :reason==KU_PG_TYPED_OID?"PostgreSQL result OID differs from the declared type"
      :reason==KU_PG_TYPED_FORMAT?"PostgreSQL typed result requires binary format"
      :reason==KU_PG_TYPED_LENGTH?"PostgreSQL binary value has an invalid length"
      :reason==KU_PG_TYPED_SCHEMA?"PostgreSQL typed declaration or parameter shape is invalid"
      :"PostgreSQL value does not match its declared type";
  return ku_pg_static_error(code,strlen(code),message,strlen(message));
}
static uint32_t ku_pg_typed_type(KuString type) {
  if(!type.ptr || !type.len || type.len>8u)return 0;
  size_t n=type.len;uint32_t nullable=type.ptr[n-1u]=='?'?KU_PG_TYPED_NULLABLE:0u;
  if(nullable)n--;
  uint32_t oid=n==4u && !memcmp(type.ptr,"int4",4u)?23u
      :n==4u && !memcmp(type.ptr,"int8",4u)?20u
      :n==4u && !memcmp(type.ptr,"bool",4u)?16u
      :n==4u && !memcmp(type.ptr,"text",4u)?25u
      :n==7u && !memcmp(type.ptr,"varchar",7u)?1043u:0u;
  return oid?oid|nullable:0u;
}
static int ku_pg_typed_oid_supported(uint32_t oid) {
  return oid==23u || oid==20u || oid==16u || oid==25u || oid==1043u;
}
static const char** ku_pg_typed_values(KuPgTypedQuery* query) { return (const char**)(query+1); }
static int* ku_pg_typed_lengths(KuPgTypedQuery* query) { return (int*)(ku_pg_typed_values(query)+query->parameters); }
static int* ku_pg_typed_formats(KuPgTypedQuery* query) { return ku_pg_typed_lengths(query)+query->parameters; }
static uint32_t* ku_pg_typed_parameter_oids(KuPgTypedQuery* query) { return (uint32_t*)(ku_pg_typed_formats(query)+query->parameters); }
static uint32_t* ku_pg_typed_result_types(KuPgTypedQuery* query) { return ku_pg_typed_parameter_oids(query)+query->parameters; }
static int ku_pg_typed_plan(const KuValue* params,size_t count,KuArray_str param_types,
    KuArray_str result_types,size_t* metadata,size_t* bytes,unsigned long long deadline,KuError* error) {
  if(count>KU_PG_MAX_PARAM_COUNT || count!=param_types.len || (count && (!params || !param_types.data))
      || result_types.len>KU_PG_MAX_RESULT_COLS || (result_types.len && !result_types.data))goto invalid;
  if(count>(SIZE_MAX-sizeof(KuPgTypedQuery))/(sizeof(char*)+3u*sizeof(uint32_t)))goto invalid;
  *metadata=sizeof(KuPgTypedQuery)+count*(sizeof(char*)+3u*sizeof(uint32_t));
  if(result_types.len>(SIZE_MAX-*metadata)/sizeof(uint32_t))goto invalid;
  *metadata+=result_types.len*sizeof(uint32_t);*bytes=0;
  for(size_t i=0;i<result_types.len;i++) {
    if(ku_pg_deadline_expired(deadline))goto expired;
    if(!ku_pg_typed_type(result_types.data[i]))goto invalid;
  }
  for(size_t i=0;i<count;i++) {
    if(ku_pg_deadline_expired(deadline))goto expired;
    uint32_t type=ku_pg_typed_type(param_types.data[i]),oid=type&KU_PG_TYPED_OID_MASK;
    if(!type)goto invalid;
    if(params[i].tag==KU_NULL) {if(!(type&KU_PG_TYPED_NULLABLE))goto invalid;continue;}
    size_t n=oid==23u?4u:oid==20u?8u:oid==16u?1u:0u;
    if(oid==23u || oid==20u) {
      if(params[i].tag!=KU_INT || (oid==23u && (params[i].as.i<INT32_MIN || params[i].as.i>INT32_MAX)))goto invalid;
    } else if(oid==16u) {if(params[i].tag!=KU_BOOL)goto invalid;}
    else {
      if(params[i].tag!=KU_STR || params[i].as.s.len>INT_MAX
          || params[i].as.s.len>KU_PG_MAX_PARAM_BYTES || (params[i].as.s.len && !params[i].as.s.ptr))goto invalid;
      n=params[i].as.s.len;
      int nul=ku_pg_string_has_nul_until(params[i].as.s,deadline);
      if(nul<0)goto expired;if(nul)goto invalid;
      int valid=ku_pg_utf8_valid(params[i].as.s.ptr,n,deadline);
      if(valid<0)goto expired;if(!valid)goto invalid;
    }
    if(n>KU_PG_MAX_PARAM_BYTES-*bytes)goto invalid;*bytes+=n;
  }
  return 1;
invalid:*error=ku_pg_typed_error(KU_PG_TYPED_SCHEMA);return 0;
expired:*error=ku_pg_query_timeout_error();return 0;
}
static int ku_pg_typed_bind(KuPgTypedQuery* query,const KuValue* params,size_t count,
    KuArray_str param_types,KuArray_str result_types,char* bytes,unsigned long long deadline) {
  query->parameters=count;query->columns=result_types.len;query->rows_seen=0;
  const char** values=ku_pg_typed_values(query);int* lengths=ku_pg_typed_lengths(query);
  int* formats=ku_pg_typed_formats(query);uint32_t* oids=ku_pg_typed_parameter_oids(query);
  uint32_t* columns=ku_pg_typed_result_types(query);
  for(size_t i=0;i<result_types.len;i++) {
    if(ku_pg_deadline_expired(deadline))return 0;
    columns[i]=ku_pg_typed_type(result_types.data[i]);
  }
  for(size_t i=0;i<count;i++) {
    if(ku_pg_deadline_expired(deadline))return 0;
    uint32_t oid=ku_pg_typed_type(param_types.data[i])&KU_PG_TYPED_OID_MASK;
    oids[i]=oid;formats[i]=1;lengths[i]=0;values[i]=NULL;
    if(params[i].tag==KU_NULL)continue;
    values[i]=bytes;
    if(oid==23u || oid==20u) {
      size_t n=oid==23u?4u:8u;uint64_t value=(uint64_t)params[i].as.i;
      for(size_t j=0;j<n;j++)bytes[j]=(char)(value>>((n-1u-j)*8u));
      lengths[i]=(int)n;bytes+=n;
    } else if(oid==16u) {bytes[0]=params[i].as.b?1:0;lengths[i]=1;bytes++;}
    else {
      size_t n=params[i].as.s.len;
      if(n && !ku_pg_copy_until(bytes,params[i].as.s.ptr,n,deadline))return 0;
      lengths[i]=(int)n;bytes+=n;
    }
  }
  return 1;
}
/* One allocation owns only the declared single row: schema, cells and bytes.
 * Schema/cells are initialized one column at a time by the bounded copier. */
static int ku_pg_typed_result_size(size_t columns,size_t bytes,size_t* tail) {
  const size_t stride=sizeof(uint32_t)+sizeof(KuPgCell);
  if(columns>KU_PG_MAX_RESULT_COLS || columns>KU_PG_MAX_RESULT_CELLS
      || bytes>KU_PG_MAX_RESULT_BYTES || columns>(SIZE_MAX-bytes)/stride)return 0;
  *tail=columns*stride+bytes;
  return *tail<KU_PG_RESULT_TYPED_FLAG && *tail<=SIZE_MAX-sizeof(KuPgResult);
}
static uint32_t* ku_pg_typed_owner_types(KuPgResult* result) { return (uint32_t*)(result+1); }
static KuPgResult* ku_pg_typed_result_new(size_t columns,size_t bytes) {
  size_t tail=0;if(!ku_pg_typed_result_size(columns,bytes,&tail))return NULL;
  KuPgResult* result=(KuPgResult*)malloc(sizeof(*result)+tail);if(!result)return NULL;
  *result=(KuPgResult){0};result->cols=columns;result->cell_capacity=columns;result->bytes_capacity=bytes;
  result->inline_storage=KU_PG_RESULT_TYPED_FLAG|tail;
  result->cells=columns?(KuPgCell*)(ku_pg_typed_owner_types(result)+columns):NULL;
  result->bytes=bytes?(uint8_t*)(result+1)+columns*(sizeof(uint32_t)+sizeof(KuPgCell)):NULL;
  return result;
}
static int ku_pg_typed_owned_bytes(KuPgResult* result,size_t* bytes) {
  size_t tail=0;if(result->rows>1u || result->cell_capacity!=result->cols || result->bytes_len>result->bytes_capacity
      || !ku_pg_typed_result_size(result->cols,result->bytes_capacity,&tail)
      || tail!=ku_pg_result_inline_bytes(result)
      || result->cells!=(result->cols?(KuPgCell*)(ku_pg_typed_owner_types(result)+result->cols):NULL)
      || result->bytes!=(result->bytes_capacity?(uint8_t*)(result+1)+result->cols*(sizeof(uint32_t)+sizeof(KuPgCell)):NULL))return 0;
  *bytes=sizeof(*result)+tail;return 1;
}
static int ku_pg_typed_cell(KuPgResult* result,int64_t row,int64_t col,uint32_t* type,KuPgCell* cell) {
  size_t physical=0;
  if(!result || !ku_pg_result_is_typed(result) || row!=0 || col<0 || (uint64_t)col>=result->cols
      || result->rows!=1u || !ku_pg_typed_owned_bytes(result,&physical))return 0;
  *type=ku_pg_typed_owner_types(result)[(size_t)col];*cell=result->cells[(size_t)col];
  if(!ku_pg_typed_oid_supported(*type&KU_PG_TYPED_OID_MASK))return 0;
  return cell->len==KU_PG_NULL_CELL || ((size_t)cell->offset<=result->bytes_len
      && (size_t)cell->len<=result->bytes_len-(size_t)cell->offset);
}
static KuResult_int ku_pg_typed_int(KuPgResult* result,int64_t row,int64_t col) {
  uint32_t type=0;KuPgCell cell={0};
  if(!ku_pg_typed_cell(result,row,col,&type,&cell))return (KuResult_int){false,0,ku_pg_value_error()};
  if(cell.len==KU_PG_NULL_CELL)return (KuResult_int){false,0,ku_pg_typed_error(KU_PG_TYPED_NULL)};
  uint32_t oid=type&KU_PG_TYPED_OID_MASK;size_t n=oid==23u?4u:oid==20u?8u:0u;
  if(!n || cell.len!=n)return (KuResult_int){false,0,ku_pg_typed_error(KU_PG_TYPED_VALUE)};
  const uint8_t* data=result->bytes+cell.offset;uint64_t value=0;
  for(size_t i=0;i<n;i++)value=(value<<8u)|data[i];
  int64_t number=n==4u ? (int64_t)value-((value&UINT64_C(0x80000000))?INT64_C(4294967296):0)
      :value>INT64_MAX?-(int64_t)(~value)-1:(int64_t)value;
  return (KuResult_int){true,number,{0}};
}
static KuResult_bool ku_pg_value_bool(KuPgResult* result,int64_t row,int64_t col) {
  uint32_t type=0;KuPgCell cell={0};
  if(!ku_pg_typed_cell(result,row,col,&type,&cell))return (KuResult_bool){false,0,ku_pg_value_error()};
  if(cell.len==KU_PG_NULL_CELL)return (KuResult_bool){false,0,ku_pg_typed_error(KU_PG_TYPED_NULL)};
  if((type&KU_PG_TYPED_OID_MASK)!=16u || cell.len!=1u || result->bytes[cell.offset]>1u)
    return (KuResult_bool){false,0,ku_pg_typed_error(KU_PG_TYPED_VALUE)};
  return (KuResult_bool){true,result->bytes[cell.offset]!=0u,{0}};
}
static int ku_pg_typed_is_text(KuPgResult* result,int64_t row,int64_t col) {
  uint32_t type=0;KuPgCell cell={0};
  return ku_pg_typed_cell(result,row,col,&type,&cell)
      && ((type&KU_PG_TYPED_OID_MASK)==25u || (type&KU_PG_TYPED_OID_MASK)==1043u);
}
