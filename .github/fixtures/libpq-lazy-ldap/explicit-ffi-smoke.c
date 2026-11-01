/* Link to rebuilt libpq A or B. No server is started and no SQL is issued.
 * Lookup cases make finite real LDAP connections to an unserved loopback port.
 * POSIX dlsym function types match the public OpenLDAP/GnuTLS C ABI.
 * This deliberately tests real DSOs; it must never be replaced with stubs.
 */
#define _GNU_SOURCE 1
#define _DARWIN_C_SOURCE 1
#include <dlfcn.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

extern int PQlibVersion(void);
extern void *PQconnectStartParams(const char *const *, const char *const *, int);
extern int PQstatus(const void *);
extern char *PQerrorMessage(const void *);
extern void PQfinish(void *);

static void save_maps(const char *stage)
{
    const char *dir = getenv("KU_FFI_MAPS_DIR");
    if (!dir) return;
    char name[4096], line[4096];
    int n = snprintf(name, sizeof(name), "%s/%s.maps", dir, stage);
    if (n < 0 || (size_t)n >= sizeof(name)) exit(2);
    FILE *input = fopen("/proc/self/maps", "r"), *output = fopen(name, "w");
    if (!input || !output) exit(2);
    while (fgets(line, sizeof(line), input)) fputs(line, output);
    fclose(input); fclose(output);
}

/* No SQL is issued. The nonexistent Unix socket directory ensures that the
 * independent connection attempt cannot reach a database after LDAP fallback.
 */
static int lookup_once(const char *expected)
{
    const char *keys[] = {"service", "host", "port", NULL};
    const char *values[] = {"ku-loader", "/nonexistent-ku-loader-gate", "1", NULL};
    void *conn = PQconnectStartParams(keys, values, 0);
    if (!conn) return 1;
    const char *error = PQerrorMessage(conn);
    int ok = PQstatus(conn) == 1;
    if (expected) ok = ok && error && strstr(error, expected);
    else ok = ok && error && strstr(error, "/nonexistent-ku-loader-gate")
        && !strstr(error, "could not load LDAP library")
        && !strstr(error, "LDAP library lacks required symbol");
    if (!ok) fprintf(stderr, "unexpected connection result: %s\n", error ? error : "null");
    PQfinish(conn);
    return !ok;
}

static void *lookup_thread(void *arg)
{
    *(int *)arg = lookup_once(NULL);
    return NULL;
}

static void parallel_first(void)
{
    pthread_t threads[8];
    int results[8] = {0};
    for (int i = 0; i < 8; ++i)
        if (pthread_create(&threads[i], NULL, lookup_thread, &results[i])) exit(2);
    for (int i = 0; i < 8; ++i) {
        if (pthread_join(threads[i], NULL) || results[i]) exit(3);
    }
    puts("{\"stage\":\"parallel-first\",\"independent_clients\":8,\"sql_issued\":0,\"all_expected_fallback\":true}");
}

static void *required(void *module, const char *name)
{
    void *symbol = dlsym(module, name);
    if (!symbol) { fprintf(stderr, "missing symbol %s\n", name); exit(2); }
    return symbol;
}

static void check_gnutls(void)
{
    void *module = dlopen("libgnutls.so.30", RTLD_NOW | RTLD_GLOBAL);
    if (!module) { fprintf(stderr, "cannot load original GnuTLS DSO\n"); exit(2); }
    int (*crt_init)(void **) = required(module, "gnutls_x509_crt_init");
    void (*crt_deinit)(void *) = required(module, "gnutls_x509_crt_deinit");
    int (*session_init)(void **, unsigned int) = required(module, "gnutls_init");
    void (*session_deinit)(void *) = required(module, "gnutls_deinit");
    void *crt = NULL, *session = NULL;
    int x = crt_init(&crt);
    int s = session_init(&session, 2U); /* GNUTLS_CLIENT */
    printf("{\"stage\":\"gnutls\",\"x509_init\":%d,\"session_init\":%d}\n", x, s);
    if (crt) crt_deinit(crt);
    if (session) session_deinit(session);
    if (x != 0 || s != 0) exit(3);
    save_maps("after-gnutls");
    /* Retain the module, exactly as the candidate does. */
}

static void check_ldap(void)
{
    void *module = dlopen("libldap.so.2", RTLD_NOW | RTLD_GLOBAL);
    if (!module) { fprintf(stderr, "cannot load original LDAP DSO\n"); exit(2); }
    int (*initialize)(void **, const char *) = required(module, "ldap_initialize");
    int (*unbind)(void *, void *, void *) = required(module, "ldap_unbind_ext_s");
    void *ldap = NULL;
    /* initialize parses the URI/creates a handle; it does not bind/search. */
    int i = initialize(&ldap, "ldaps://127.0.0.1:1");
    int u = ldap ? unbind(ldap, NULL, NULL) : -1;
    printf("{\"stage\":\"ldap\",\"ldaps_handle_init\":%d,\"unbind\":%d}\n", i, u);
    if (i != 0 || u != 0) exit(3);
    save_maps("after-ldap");
}

int main(int argc, char **argv)
{
    if (getenv("GNUTLS_SKIP_GLOBAL_INIT")) {
        fprintf(stderr, "GNUTLS_SKIP_GLOBAL_INIT must be absent\n"); return 2;
    }
    int version = PQlibVersion();
    printf("{\"stage\":\"initial\",\"pq_version\":%d,\"ambient_ldap\":%s,\"ambient_gnutls\":%s}\n",
           version, dlsym(RTLD_DEFAULT, "ldap_init") ? "true" : "false",
           dlsym(RTLD_DEFAULT, "gnutls_init") ? "true" : "false");
    save_maps("initial");
    if (version != 180006 || argc != 2) return 2;
    if (!strcmp(argv[1], "plain")) return 0;
    if (!strcmp(argv[1], "parallel-first")) { parallel_first(); check_gnutls(); }
    else if (!strcmp(argv[1], "missing-dso") || !strcmp(argv[1], "missing-symbol")) {
        const char *expected = !strcmp(argv[1], "missing-dso")
            ? "could not load LDAP library libldap.so.2"
            : "LDAP library lacks required symbol ldap_value_free_len";
        if (lookup_once(expected)) return 3;
        printf("{\"stage\":\"injected-failure\",\"kind\":\"%s\",\"expected_error\":true}\n", argv[1]);
        if (unsetenv("KU_LDAP_FAULT")) return 2;
        if (lookup_once(NULL)) return 3;
        puts("{\"stage\":\"later-independent-client\",\"loader_recovered\":true,\"sql_issued\":0}");
        check_gnutls();
    }
    else if (!strcmp(argv[1], "ldap-first")) { check_ldap(); check_gnutls(); }
    else if (!strcmp(argv[1], "gnutls-first")) { check_gnutls(); check_ldap(); }
    else return 2;
    return 0;
}
