
static KuString ku_string_unicode_trim(KuString s) {
  size_t at=0,start=0,end=0,fuel=0;int found=0;
  while(at<s.len){
    if(!ku_unicode_continue(&fuel))return (KuString){0};
    size_t before=at;uint32_t cp=ku_unicode_read(s,&at);
    if(!ku_unicode_class(ku_unicode_space,sizeof(ku_unicode_space)/sizeof(*ku_unicode_space),cp)){
      if(!found){start=before;found=1;}end=at;
    }
  }
  KuString result=ku_string_alloc(end-start);
  if(result.len)memcpy(result.ptr,s.ptr+start,result.len);
  return result;
}
