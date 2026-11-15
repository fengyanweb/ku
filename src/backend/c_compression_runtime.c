/* Optional synchronous byte codecs. The exact header and library versions are
 * part of the native dependency contract, not inferred from a library name.
 * No context, allocation, thread or lazy global is created before a call.
 *
 * Algorithms and upstream interfaces:
 * Zstandard 1.5.7, BSD-3-Clause (upstream LICENSE),
 * zlib 1.4, zlib license (upstream LICENSE).
 * Upstream headers retain their original notices in the optional SDK.
 */
#include <stddef.h>

#if defined(KU_FEATURE_ZSTD)
#include <zstd.h>
#include <zstd_errors.h>
#if ZSTD_VERSION_NUMBER != 10507
#error "std.zstd requires the frozen Zstandard 1.5.7 headers"
#endif
#endif
#if defined(KU_FEATURE_ZLIB)
#include <zlib.h>
#if ZLIB_VERNUM != 0x1400
#error "std.gzip requires the frozen zlib 1.4 headers"
#endif
#endif

#define KU_COMPRESSION_CHUNK 32768u
#define KU_COMPRESSION_OUTPUT_MAX (64u * 1024u * 1024u)
#define KU_GZIP_WORKSPACE_MAX (2u * 1024u * 1024u)

typedef union KuCompressionAllocation KuCompressionAllocation;
union KuCompressionAllocation {
  struct {
    size_t bytes;
    KuCompressionAllocation* previous;
    KuCompressionAllocation* next;
  } record;
  /* The supported 32/64-bit targets retain malloc's alignment without relying
   * on C library max_align_t (not exposed by some supported MSVC headers). */
  long double scalar_alignment;
  uint64_t integer_alignment;
  void* pointer_alignment;
  uint8_t alignment_padding[32];
};
_Static_assert(sizeof(void*) <= 8u && sizeof(KuCompressionAllocation) % 16u == 0u,
    "compression allocation header must preserve native malloc alignment");
typedef struct KuCompressionWorkspace {
  size_t live, limit;
  int failure; /* 1: quota; 2: allocator; 3: unexpected retained library block */
  KuCompressionAllocation* first;
} KuCompressionWorkspace;
typedef struct KuCompressionOutput {
  KuBytes bytes;
  size_t limit;
} KuCompressionOutput;

static void* ku_compression_allocate(void* raw, size_t bytes) {
  KuCompressionWorkspace* workspace = (KuCompressionWorkspace*)raw;
  if (!workspace) return NULL;
  if (!bytes) bytes = 1u;
  if (bytes > SIZE_MAX - sizeof(KuCompressionAllocation)
      || workspace->live > workspace->limit
      || bytes + sizeof(KuCompressionAllocation) > workspace->limit - workspace->live) {
    workspace->failure = 1;
    return NULL;
  }
  size_t charged = bytes + sizeof(KuCompressionAllocation);
  KuCompressionAllocation* allocation = (KuCompressionAllocation*)malloc(charged);
  if (!allocation) {
    workspace->failure = 2;
    return NULL;
  }
  allocation->record.bytes = charged;
  allocation->record.previous = NULL;
  allocation->record.next = workspace->first;
  if (workspace->first) workspace->first->record.previous = allocation;
  workspace->first = allocation;
  workspace->live += charged;
  return allocation + 1;
}
static void ku_compression_free(void* raw, void* value) {
  KuCompressionWorkspace* workspace = (KuCompressionWorkspace*)raw;
  if (!workspace || !value) return;
  KuCompressionAllocation* allocation = ((KuCompressionAllocation*)value) - 1;
  if (allocation->record.previous)
    allocation->record.previous->record.next = allocation->record.next;
  else workspace->first = allocation->record.next;
  if (allocation->record.next)
    allocation->record.next->record.previous = allocation->record.previous;
  workspace->live -= allocation->record.bytes;
  free(allocation);
}
/* Only after the library context has been destroyed. An unexpected retained
 * block becomes an explicit error; it must not survive a failed operation. */
static void ku_compression_workspace_finish(KuCompressionWorkspace* workspace) {
  if (workspace->first && !workspace->failure) workspace->failure = 3;
  while (workspace->first) ku_compression_free(workspace, workspace->first + 1);
}
static KuError ku_compression_error(const char* domain, const char* code, const char* message) {
  return ku_error_make(
      ku_string_static((const uint8_t*)domain, strlen(domain)),
      ku_string_static((const uint8_t*)code, strlen(code)),
      ku_string_static((const uint8_t*)message, strlen(message)));
}
static int ku_compression_arguments(KuBytes input, int64_t limit) {
  return limit >= 0 && (uint64_t)limit <= (uint64_t)KU_COMPRESSION_OUTPUT_MAX
      && input.len <= (size_t)KU_BYTES_MAX_LENGTH && input.len <= input.capacity
      && (input.len == 0u || input.ptr != NULL)
      && (input.storage == KU_BYTES_STATIC || input.storage == KU_BYTES_OWNED);
}
/* At the exact output limit one scratch byte is still allowed: the decoder
 * may consume a checksum/footer without output, or prove the limit exceeded.
 * This never grows the owned output past its declared limit. */
static size_t ku_compression_chunk_size(const KuCompressionOutput* output) {
  size_t available = output->limit - output->bytes.len + 1u;
  return available < KU_COMPRESSION_CHUNK ? available : KU_COMPRESSION_CHUNK;
}
static int ku_compression_append(KuCompressionOutput* output, const uint8_t* bytes, size_t count) {
  if (count > output->limit - output->bytes.len) return 1;
  if (!count) return 0;
  size_t required = output->bytes.len + count;
  if (required > output->bytes.capacity) {
    size_t capacity = output->bytes.capacity;
    if (!capacity) capacity = output->limit < 4096u ? output->limit : 4096u;
    while (capacity < required)
      capacity = capacity > output->limit / 2u ? output->limit : capacity * 2u;
    uint8_t* replacement = (uint8_t*)realloc(output->bytes.ptr, capacity);
    if (!replacement) return 2;
    output->bytes.ptr = replacement;
    output->bytes.capacity = capacity;
    output->bytes.storage = KU_BYTES_OWNED;
  }
  memcpy(output->bytes.ptr + output->bytes.len, bytes, count);
  output->bytes.len += count;
  return 0;
}
static KuResult_bytes ku_compression_finish(const char* domain, KuCompressionOutput* output,
    KuCompressionWorkspace* workspace, const char* code, const char* message) {
  ku_compression_workspace_finish(workspace);
  if (!code && workspace->failure) {
    code = workspace->failure == 1 ? "workspace_limit" : workspace->failure == 2
        ? "out_of_memory" : "internal_error";
    message = "compression workspace allocation or cleanup failed";
  }
  if (code) {
    ku_drop_bytes(&output->bytes);
    return (KuResult_bytes){ false, (KuBytes){0}, ku_compression_error(domain, code, message) };
  }
  KuBytes value = ku_move_bytes(&output->bytes);
  return (KuResult_bytes){ true, value, (KuError){0} };
}

#if defined(KU_FEATURE_ZSTD)
static void ku_zstd_error(size_t status, const char** code, const char** message) {
  if (ZSTD_getErrorCode(status) == ZSTD_error_memory_allocation) {
    *code = "out_of_memory";
    *message = "Zstandard workspace could not be allocated";
  } else if (ZSTD_getErrorCode(status) == ZSTD_error_frameParameter_windowTooLarge) {
    *code = "window_limit";
    *message = "Zstandard frame window exceeds the upstream default 128 MiB limit";
  } else if (ZSTD_getErrorCode(status) == ZSTD_error_dictionary_wrong
      || ZSTD_getErrorCode(status) == ZSTD_error_dictionary_corrupted) {
    *code = "dictionary_required";
    *message = "Zstandard dictionaries are not supported by this interface";
  } else {
    *code = "invalid_data";
    *message = "Zstandard frame is invalid, corrupt or uses an unsupported parameter";
  }
}
static KuResult_bytes ku_zstd_compress(KuBytes input, int64_t max_output_bytes) {
  const char* domain = "zstd";
  KuCompressionOutput output = {{0}, 0u};
  KuCompressionWorkspace workspace = {0};
  const char *code = NULL, *message = NULL;
  if (!ku_compression_arguments(input, max_output_bytes))
    return (KuResult_bytes){ false, (KuBytes){0}, ku_compression_error(domain,
      "invalid_argument", "compression requires valid bytes and a limit from 0 to 64 MiB") };
  output.limit = (size_t)max_output_bytes;
  if (ZSTD_versionNumber() != 10507u)
    return (KuResult_bytes){ false, (KuBytes){0}, ku_compression_error(domain,
      "library_version_mismatch", "std.zstd requires Zstandard 1.5.7") };
  ZSTD_CCtx* context = ZSTD_createCCtx();
  if (!context) {
    code = "out_of_memory";
    message = "Zstandard context allocation failed";
  } else {
    size_t status = ZSTD_CCtx_setParameter(context, ZSTD_c_compressionLevel, 3);
    if (!ZSTD_isError(status)) status = ZSTD_CCtx_setParameter(context, ZSTD_c_checksumFlag, 1);
    if (!ZSTD_isError(status)) status = ZSTD_CCtx_setParameter(context, ZSTD_c_nbWorkers, 0);
    if (!ZSTD_isError(status)) status = ZSTD_CCtx_setPledgedSrcSize(context, input.len);
    if (ZSTD_isError(status)) ku_zstd_error(status, &code, &message);
    ZSTD_inBuffer source = { input.ptr, input.len, 0u };
    uint8_t scratch[KU_COMPRESSION_CHUNK];
    while (!code) {
      size_t before = source.pos;
      ZSTD_outBuffer destination = { scratch, ku_compression_chunk_size(&output), 0u };
      status = ZSTD_compressStream2(context, &destination, &source, ZSTD_e_end);
      if (ZSTD_isError(status)) { ku_zstd_error(status, &code, &message); break; }
      int appended = ku_compression_append(&output, scratch, destination.pos);
      if (appended) {
        code = appended == 1 ? "output_limit" : "out_of_memory";
        message = "Zstandard output exceeds its limit or cannot be allocated";
      } else if (!status) {
        if (source.pos != source.size) { code = "internal_error"; message = "Zstandard did not consume the input"; }
        break;
      } else if (source.pos == before && !destination.pos) {
        code = "internal_error"; message = "Zstandard encoder made no progress";
      }
    }
    ZSTD_freeCCtx(context);
  }
  return ku_compression_finish(domain, &output, &workspace, code, message);
}
static KuResult_bytes ku_zstd_decompress(KuBytes input, int64_t max_output_bytes) {
  const char* domain = "zstd";
  KuCompressionOutput output = {{0}, 0u};
  KuCompressionWorkspace workspace = {0};
  const char *code = NULL, *message = NULL;
  if (!ku_compression_arguments(input, max_output_bytes))
    return (KuResult_bytes){ false, (KuBytes){0}, ku_compression_error(domain,
      "invalid_argument", "decompression requires valid bytes and a limit from 0 to 64 MiB") };
  output.limit = (size_t)max_output_bytes;
  if (ZSTD_versionNumber() != 10507u)
    return (KuResult_bytes){ false, (KuBytes){0}, ku_compression_error(domain,
      "library_version_mismatch", "std.zstd requires Zstandard 1.5.7") };
  ZSTD_DCtx* context = ZSTD_createDCtx();
  if (!context) {
    code = "out_of_memory";
    message = "Zstandard context allocation failed";
  } else {
    /* Preserve the pinned library's default window cap (128 MiB in 1.5.7).
     * This is separate from output bytes, context/entropy-table allocations,
     * allocator overhead and total process memory. */
    size_t status = ZSTD_initDStream(context);
    if (!ZSTD_isError(status)) status = ZSTD_DCtx_setParameter(context, ZSTD_d_windowLogMax, 0);
    if (ZSTD_isError(status)) ku_zstd_error(status, &code, &message);
    ZSTD_inBuffer source = { input.ptr, input.len, 0u };
    uint8_t scratch[KU_COMPRESSION_CHUNK];
    while (!code) {
      size_t before = source.pos;
      ZSTD_outBuffer destination = { scratch, ku_compression_chunk_size(&output), 0u };
      status = ZSTD_decompressStream(context, &destination, &source);
      if (ZSTD_isError(status)) { ku_zstd_error(status, &code, &message); break; }
      int appended = ku_compression_append(&output, scratch, destination.pos);
      if (appended) {
        code = appended == 1 ? "output_limit" : "out_of_memory";
        message = "Zstandard output exceeds its limit or cannot be allocated";
      } else if (!status && source.pos == source.size) {
        break;
      } else if (source.pos == before && !destination.pos) {
        code = source.pos == source.size ? "truncated_input" : "invalid_data";
        message = "Zstandard input ended before a complete frame or made no progress";
      }
    }
    ZSTD_freeDCtx(context);
  }
  return ku_compression_finish(domain, &output, &workspace, code, message);
}
#endif

#if defined(KU_FEATURE_ZLIB)
static voidpf ku_gzip_allocate(voidpf raw, uInt count, uInt size) {
  if (count && (size_t)size > SIZE_MAX / (size_t)count) {
    ((KuCompressionWorkspace*)raw)->failure = 1;
    return NULL;
  }
  size_t bytes = (size_t)count * (size_t)size;
  void* allocation = ku_compression_allocate(raw, bytes);
  if (allocation && bytes) memset(allocation, 0, bytes);
  return allocation;
}
static void ku_gzip_free(voidpf raw, voidpf pointer) { ku_compression_free(raw, pointer); }
static KuResult_bytes ku_zlib_codec(KuBytes input, int64_t max_output_bytes, int encode, int gzip) {
  const char* domain = gzip ? "gzip" : "deflate";
  KuCompressionOutput output = {{0}, 0u};
  KuCompressionWorkspace workspace = {0u, KU_GZIP_WORKSPACE_MAX, 0, NULL};
  const char *code = NULL, *message = NULL;
  if (!ku_compression_arguments(input, max_output_bytes) || input.len > UINT_MAX)
    return (KuResult_bytes){ false, (KuBytes){0}, ku_compression_error(domain,
      "invalid_argument", "compression requires valid bytes and a limit from 0 to 64 MiB") };
  output.limit = (size_t)max_output_bytes;
  if (strcmp(zlibVersion(), ZLIB_VERSION) != 0)
    return (KuResult_bytes){ false, (KuBytes){0}, ku_compression_error(domain,
      "library_version_mismatch", "std.gzip and std.deflate require zlib 1.4") };
  z_stream stream = {0};
  stream.zalloc = ku_gzip_allocate;
  stream.zfree = ku_gzip_free;
  stream.opaque = &workspace;
  int window = gzip ? 15 + 16 : -15;
  int status = encode ? deflateInit2(&stream, Z_DEFAULT_COMPRESSION, Z_DEFLATED,
      window, 8, Z_DEFAULT_STRATEGY) : inflateInit2(&stream, window);
  stream.next_in = (Bytef*)input.ptr;
  stream.avail_in = (uInt)input.len;
  if (status != Z_OK) {
    code = workspace.failure == 1 ? "workspace_limit" : status == Z_MEM_ERROR
        ? "out_of_memory" : "internal_error";
    message = "DEFLATE context could not be initialized";
  }
  uint8_t scratch[KU_COMPRESSION_CHUNK];
  while (!code) {
    uInt before = stream.avail_in;
    size_t capacity = ku_compression_chunk_size(&output);
    stream.next_out = scratch;
    stream.avail_out = (uInt)capacity;
    status = encode ? deflate(&stream, Z_FINISH) : inflate(&stream, Z_NO_FLUSH);
    size_t produced = capacity - stream.avail_out;
    int appended = ku_compression_append(&output, scratch, produced);
    if (appended) {
      code = appended == 1 ? "output_limit" : "out_of_memory";
      message = "DEFLATE output exceeds its limit or cannot be allocated";
    } else if (status == Z_STREAM_END) {
      if (!stream.avail_in) break;
      if (encode) { code = "internal_error"; message = "DEFLATE encoder did not consume the input"; }
      else if (!gzip) {
        code = "invalid_data"; message = "raw DEFLATE input contains trailing data";
      } else {
        /* Concatenated members share one cumulative output bound. Header,
         * checksum and trailer checks remain enabled for every member. */
        Bytef* remaining = stream.next_in;
        uInt count = stream.avail_in;
        status = inflateReset2(&stream, window);
        stream.next_in = remaining;
        stream.avail_in = count;
        if (status != Z_OK) { code = "internal_error"; message = "gzip member reset failed"; }
      }
    } else if (status == Z_NEED_DICT) {
      code = "dictionary_required"; message = "DEFLATE dictionaries are not supported";
    } else if (status == Z_MEM_ERROR) {
      code = workspace.failure == 1 ? "workspace_limit" : "out_of_memory";
      message = "DEFLATE workspace could not be allocated within its limit";
    } else if (status != Z_OK && status != Z_BUF_ERROR) {
      code = "invalid_data";
      message = gzip ? "gzip header, payload or checksum is invalid" : "raw DEFLATE stream is invalid";
    } else if (before == stream.avail_in && !produced) {
      code = !encode && !stream.avail_in ? "truncated_input" : "invalid_data";
      message = gzip ? "gzip input ended before a complete member or made no progress"
          : "raw DEFLATE input ended before a complete stream or made no progress";
    }
  }
  if (stream.state) {
    if (encode) deflateEnd(&stream);
    else inflateEnd(&stream);
  }
  return ku_compression_finish(domain, &output, &workspace, code, message);
}
static KuResult_bytes ku_gzip_compress(KuBytes input, int64_t max_output_bytes) {
  return ku_zlib_codec(input, max_output_bytes, 1, 1);
}
static KuResult_bytes ku_gzip_decompress(KuBytes input, int64_t max_output_bytes) {
  return ku_zlib_codec(input, max_output_bytes, 0, 1);
}
static KuResult_bytes ku_deflate_compress(KuBytes input, int64_t max_output_bytes) {
  return ku_zlib_codec(input, max_output_bytes, 1, 0);
}
static KuResult_bytes ku_deflate_decompress(KuBytes input, int64_t max_output_bytes) {
  return ku_zlib_codec(input, max_output_bytes, 0, 0);
}
#endif
