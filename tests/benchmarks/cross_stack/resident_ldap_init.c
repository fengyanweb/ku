#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

/* Diagnostic only. Public OpenLDAP2.6 ABI constants from include/ldap.h;
 * opaque pointers avoid requiring development headers on the normal runner.
 * Links the exact original libpq to keep its real dependency constructors. */
extern int PQlibVersion(void);
typedef struct ldap LDAP;
enum { LDAP_OPT_X_TLS_NEWCTX=0x600f,
       LDAP_OPT_X_TLS_PACKAGE=0x6011, GNUTLS_CLIENT=2 };
static void *required(void *handle,const char *name) {
  void *symbol=dlsym(handle,name);
  if(!symbol) {fprintf(stderr,"missing public symbol: %s\n",name);_Exit(93);}
  return symbol;
}
static void checkpoint(const char *stage) {
  char next;
  printf("{\"stage\":\"%s\"}\n",stage);fflush(stdout);
  if(read(STDIN_FILENO,&next,1)!=1 || next!='x')_Exit(94);
}
int main(int argc,char **argv) {
  void *ldap=dlopen("libldap.so.2",RTLD_NOW|RTLD_NOLOAD);
  void *tls=dlopen("libgnutls.so.30",RTLD_NOW|RTLD_NOLOAD);
  if(!ldap || !tls)return 95;
  int (*initialize)(LDAP **,const char *)=required(ldap,"ldap_initialize");
  int (*set)(LDAP *,int,const void *)=required(ldap,"ldap_set_option");
  int (*get)(LDAP *,int,void *)=required(ldap,"ldap_get_option");
  int (*unbind)(LDAP *,void *,void *)=required(ldap,"ldap_unbind_ext_s");
  void (*ldap_free)(void *)=required(ldap,"ldap_memfree");
  int (*tls_init)(void **,unsigned)=required(tls,"gnutls_init");
  void (*tls_deinit)(void *)=required(tls,"gnutls_deinit");
  int (*priority)(void *)=required(tls,"gnutls_set_default_priority");
  int (*x509_init)(void **)=required(tls,"gnutls_x509_crt_init");
  void (*x509_deinit)(void *)=required(tls,"gnutls_x509_crt_deinit");
  const char *(*version)(const char *)=required(tls,"gnutls_check_version");
  printf("{\"libpq_version\":%d,\"gnutls_version\":\"%s\"}\n",PQlibVersion(),version(NULL));
  checkpoint("loaded-before-explicit-init");
  if(argc==2 && !strcmp(argv[1],"ffi-without-explicit-init")) {
    void *session=NULL,*certificate=NULL;int cert_rc=x509_init(&certificate);
    if(!cert_rc)x509_deinit(certificate);
    int rc=tls_init(&session,GNUTLS_CLIENT),pr=rc;
    if(!rc) {pr=priority(session);tls_deinit(session);}
    printf("{\"direct_ffi_x509\":%d,\"direct_ffi_init\":%d,\"direct_ffi_priority\":%d}\n",cert_rc,rc,pr);
    checkpoint("direct-ffi-result");dlclose(ldap);dlclose(tls);return 0;
  }
  LDAP *ld=NULL;char *package=NULL;int client=0;
  int created=initialize(&ld,"ldap://127.0.0.1:9"); /* no connect/bind operation */
  int newctx=created ? created:set(ld,LDAP_OPT_X_TLS_NEWCTX,&client);
  int getpackage=newctx ? newctx:get(ld,LDAP_OPT_X_TLS_PACKAGE,&package);
  void *session=NULL;int session_rc=newctx ? newctx:tls_init(&session,GNUTLS_CLIENT);
  int priority_rc=session_rc ? session_rc:priority(session);
  printf("{\"ldap_initialize\":%d,\"ldap_newctx\":%d,\"ldap_package_rc\":%d,\"ldap_package\":\"%s\",\"post_ldap_session\":%d,\"post_ldap_priority\":%d}\n",
      created,newctx,getpackage,package?package:"",session_rc,priority_rc);
  checkpoint("after-ldap-explicit-init");
  if(!session_rc)tls_deinit(session);
  if(package)ldap_free(package);
  if(ld)unbind(ld,NULL,NULL);
  dlclose(ldap);dlclose(tls);
  return created || newctx || getpackage || session_rc || priority_rc ? 96:0;
}
