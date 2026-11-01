#define _GNU_SOURCE
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#define CHECK(x) do { if (!(x)) _Exit(91); } while (0)
int main(void) {
  unsigned char *keep = malloc(37), *zero = calloc(3, 29), *old = malloc(53);
  CHECK(keep && zero && old);
  memset(keep, 0x45, 37); memset(old, 0x67, 53);
  for (size_t i = 0; i < 87; i++) CHECK(!zero[i]);
  volatile size_t huge = SIZE_MAX;
  errno = 0;
  void *failed = realloc(old, huge);
  CHECK(!failed && errno == ENOMEM && old[0] == 0x67 && old[52] == 0x67);
  errno = 0; CHECK(!calloc(huge, 2) && errno == ENOMEM);
  void *discard = malloc(21); CHECK(discard);
  CHECK(!realloc(discard, 0)); /* glibc explicitly frees this allocation. */
  void *from_null = realloc(NULL, 31); CHECK(from_null); free(from_null);
  void *aligned = (void *)(uintptr_t)7;
  errno = EDOM;
  CHECK(posix_memalign(&aligned, 3, 31) == EINVAL && aligned == (void *)(uintptr_t)7 && errno == EDOM);
  CHECK(!posix_memalign(&aligned, 64, 128) && !((uintptr_t)aligned & 63));
  free(aligned);
  for (size_t round = 0; round < 8; round++) {
    void *items[257];
    for (size_t i = 0; i < 257; i++) { items[i] = malloc(1 + i * 7); CHECK(items[i]); }
    for (size_t i = 0; i < 257; i += 2) free(items[i]);
    for (size_t i = 1; i < 257; i += 2) free(items[i]);
  }
  printf("{\"malloc\":\"%p\",\"calloc\":\"%p\",\"failed_realloc_old\":\"%p\"}\n", (void *)keep, (void *)zero, (void *)old);
  CHECK(!fflush(stdout));
  char ack; CHECK(read(STDIN_FILENO, &ack, 1) == 1 && ack == 'x');
  free(old); free(zero); free(keep);
  return 0;
}
