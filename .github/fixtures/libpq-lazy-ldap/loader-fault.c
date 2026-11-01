/* Owned single-process Linux AArch64 test seam; never linked into libpq.
 * Delegates to the real loader and changes exactly one requested failure.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static _Atomic int hits;

void *dlopen(const char *name, int flags)
{
    void *(*real)(const char *, int) = dlvsym(RTLD_NEXT, "dlopen", "GLIBC_2.17");
    if (!real) abort();
    const char *mode = getenv("KU_LDAP_FAULT");
    if (mode && !strcmp(mode, "missing-dso") && name && !strcmp(name, "libldap.so.2")) {
        atomic_fetch_add(&hits, 1);
        return NULL;
    }
    return real(name, flags);
}

void *dlsym(void *module, const char *name)
{
    void *(*real)(void *, const char *) = dlvsym(RTLD_NEXT, "dlsym", "GLIBC_2.17");
    if (!real) abort();
    const char *mode = getenv("KU_LDAP_FAULT");
    if (mode && !strcmp(mode, "missing-symbol") && !strcmp(name, "ldap_value_free_len")) {
        atomic_fetch_add(&hits, 1);
        return NULL;
    }
    return real(module, name);
}

__attribute__((destructor)) static void report_hits(void)
{
    fprintf(stderr, "KU_LDAP_FAULT_HITS=%d\n", atomic_load(&hits));
}
