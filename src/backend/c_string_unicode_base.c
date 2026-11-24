
typedef struct { uint32_t first,last; unsigned char kind; } KuUnicodeRange;
typedef struct { uint32_t codepoint,offset,length; } KuUnicodeMap;
static unsigned ku_unicode_class(const KuUnicodeRange* table,size_t count,uint32_t cp) {
  size_t lo=0,hi=count;
  while(lo<hi){size_t mid=lo+(hi-lo)/2u;if(table[mid].last<cp)lo=mid+1u;else hi=mid;}
  return lo<count && table[lo].first<=cp ? table[lo].kind : 0u;
}
static uint32_t ku_unicode_read(KuString s,size_t* at) {
  uint32_t cp=s.ptr[(*at)++];size_t extra=0;uint32_t minimum=0;
  if(cp<0x80u)return cp;
  if(cp>=0xc2u&&cp<=0xdfu){cp&=0x1fu;extra=1u;minimum=0x80u;}
  else if(cp>=0xe0u&&cp<=0xefu){cp&=0x0fu;extra=2u;minimum=0x800u;}
  else if(cp>=0xf0u&&cp<=0xf4u){cp&=7u;extra=3u;minimum=0x10000u;}
  else goto bad;
  if(extra>s.len-*at)goto bad;
  for(size_t k=0;k<extra;k++){unsigned c=s.ptr[(*at)++];if((c&0xc0u)!=0x80u)goto bad;cp=(cp<<6)|(c&0x3fu);}
  if(cp<minimum || cp>0x10ffffu || (cp>=0xd800u&&cp<=0xdfffu))goto bad;
  return cp;
bad: fputs("invalid UTF-8 in native string transform\n",stderr);exit(1);
}
static int ku_unicode_continue(size_t* fuel) {
  if(++*fuel<64u)return 1;
  *fuel=0;return !__ku_handler_timeout_poll();
}
