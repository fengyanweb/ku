/* Complete paired API forwarding, not an LDAP implementation or allocator.
 * Deliberately do not link -lldap: first real loading stays under libpq control.
 */
#define _GNU_SOURCE 1
#define LDAP_DEPRECATED 1
#include <ldap.h>
#include <dlfcn.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/time.h>

static _Atomic unsigned long long hits[12];
#define FORWARD(result_type, name, arguments, parameters, index) \
    result_type name arguments { \
        __typeof__(&name) original = dlsym(RTLD_NEXT, #name); \
        if (!original) abort(); \
        atomic_fetch_add_explicit(&hits[index], 1, memory_order_relaxed); \
        return original parameters; \
    }

FORWARD(LDAP *, ldap_init, (const char *h, int p), (h, p), 0)
FORWARD(int, ldap_set_option, (LDAP *l, int o, const void *v), (l, o, v), 1)
FORWARD(int, ldap_simple_bind, (LDAP *l, const char *w, const char *p), (l, w, p), 2)
FORWARD(int, ldap_result, (LDAP *l, int i, int a, struct timeval *t, LDAPMessage **r), (l, i, a, t, r), 3)
FORWARD(int, ldap_msgfree, (LDAPMessage *m), (m), 4)
FORWARD(int, ldap_search_st, (LDAP *l, const char *b, int s, const char *f, char **a, int o, struct timeval *t, LDAPMessage **r), (l, b, s, f, a, o, t, r), 5)
FORWARD(char *, ldap_err2string, (int e), (e), 6)
FORWARD(int, ldap_unbind, (LDAP *l), (l), 7)
FORWARD(int, ldap_count_entries, (LDAP *l, LDAPMessage *m), (l, m), 8)
FORWARD(LDAPMessage *, ldap_first_entry, (LDAP *l, LDAPMessage *m), (l, m), 9)
FORWARD(struct berval **, ldap_get_values_len, (LDAP *l, LDAPMessage *m, const char *a), (l, m, a), 10)
void ldap_value_free_len(struct berval **v)
{
    __typeof__(&ldap_value_free_len) original = dlsym(RTLD_NEXT, "ldap_value_free_len");
    if (!original) abort();
    atomic_fetch_add_explicit(&hits[11], 1, memory_order_relaxed);
    original(v);
}

__attribute__((destructor)) static void report(void)
{
    const char *names[] = {"ldap_init", "ldap_set_option", "ldap_simple_bind", "ldap_result",
        "ldap_msgfree", "ldap_search_st", "ldap_err2string", "ldap_unbind", "ldap_count_entries",
        "ldap_first_entry", "ldap_get_values_len", "ldap_value_free_len"};
    fputs("KU_LDAP_INTERPOSE={", stderr);
    for (int i = 0; i < 12; ++i)
        fprintf(stderr, "%s\"%s\":%llu", i ? "," : "", names[i], atomic_load(&hits[i]));
    fputs("}\n", stderr);
}
