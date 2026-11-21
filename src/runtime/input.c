/* Shared by the CLI interpreter (including Runner) and generated native C. */
#if !defined(_WIN32) && !defined(_POSIX_C_SOURCE)
#define _POSIX_C_SOURCE 200809L
#endif
#if defined(__APPLE__) && !defined(_DARWIN_C_SOURCE)
#define _DARWIN_C_SOURCE
#endif

#include <errno.h>
#include <limits.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#include <fcntl.h>
#include <io.h>
#include <windows.h>
#else
#include <poll.h>
#include <signal.h>
#include <termios.h>
#include <unistd.h>
#endif

#ifndef KU_INPUT_SCOPE
#define KU_INPUT_SCOPE
#endif

typedef struct {
    unsigned char *bytes;
    size_t len;
    int64_t integer;
} KuInputValue;

enum {
    __ku_input_ok,
    __ku_input_eof,
    __ku_input_io,
    __ku_input_invalid_int,
    __ku_input_alloc,
};

KU_INPUT_SCOPE void ku_input_free(KuInputValue *fields, size_t count) {
    for (size_t i = 0; i < count; ++i) {
        free(fields[i].bytes);
        fields[i] = (KuInputValue){0};
    }
}

static int __ku_input_utf8(const unsigned char *bytes, size_t len) {
    for (size_t i = 0; i < len;) {
        unsigned char first = bytes[i++];
        if (first < 0x80) continue;
        size_t tail;
        unsigned char low = 0x80, high = 0xbf;
        if (first >= 0xc2 && first <= 0xdf) {
            tail = 1;
        } else if (first >= 0xe0 && first <= 0xef) {
            tail = 2;
            if (first == 0xe0) low = 0xa0;
            if (first == 0xed) high = 0x9f;
        } else if (first >= 0xf0 && first <= 0xf4) {
            tail = 3;
            if (first == 0xf0) low = 0x90;
            if (first == 0xf4) high = 0x8f;
        } else {
            return 0;
        }
        if (len - i < tail || bytes[i] < low || bytes[i] > high) return 0;
        ++i;
        for (size_t j = 1; j < tail; ++j, ++i) {
            if ((bytes[i] & 0xc0) != 0x80) return 0;
        }
    }
    return 1;
}

static int __ku_input_chomp(KuInputValue *value) {
    if (value->len != 0 && value->bytes[value->len - 1] == '\n') {
        --value->len;
        if (value->len != 0 && value->bytes[value->len - 1] == '\r') {
            --value->len;
        }
    }
    return __ku_input_utf8(value->bytes, value->len) ? __ku_input_ok : __ku_input_io;
}

static int __ku_input_stream_line(KuInputValue *value) {
    size_t capacity = 0;
    for (;;) {
        int ch = fgetc(stdin);
        if (ch == EOF) {
            if (ferror(stdin)) return __ku_input_io;
            if (value->len == 0) return __ku_input_eof;
            break;
        }
        if (value->len == capacity) {
            if (capacity > SIZE_MAX / 2) return __ku_input_alloc;
            size_t next = capacity == 0 ? 128 : capacity * 2;
            void *buffer = realloc(value->bytes, next);
            if (buffer == NULL) return __ku_input_alloc;
            value->bytes = buffer;
            capacity = next;
        }
        value->bytes[value->len++] = (unsigned char)ch;
        if (ch == '\n') break;
    }
    return __ku_input_chomp(value);
}

static int __ku_input_integer(KuInputValue *value) {
    size_t start = 0, end = value->len;
    const unsigned char *bytes = value->bytes;
    while (start < end && (bytes[start] == ' ' || bytes[start] == '\t')) ++start;
    while (start < end && (bytes[end - 1] == ' ' || bytes[end - 1] == '\t')) --end;
    int negative = 0;
    if (start < end && (bytes[start] == '+' || bytes[start] == '-')) {
        negative = bytes[start++] == '-';
    }
    if (start == end) return __ku_input_invalid_int;
    uint64_t limit = (uint64_t)INT64_MAX + (unsigned)negative;
    uint64_t number = 0;
    for (size_t i = start; i < end; ++i) {
        if (bytes[i] < '0' || bytes[i] > '9') return __ku_input_invalid_int;
        unsigned digit = (unsigned)(bytes[i] - '0');
        if (number > (limit - digit) / 10) return __ku_input_invalid_int;
        number = number * 10 + digit;
    }
    value->integer = negative
        ? (number == limit ? INT64_MIN : -(int64_t)number)
        : (int64_t)number;
    free(value->bytes);
    value->bytes = NULL;
    value->len = 0;
    return __ku_input_ok;
}

#ifdef _WIN32
typedef HANDLE __ku_input_console;

static __ku_input_console __ku_input_console_at(DWORD handle_id) {
    HANDLE handle = GetStdHandle(handle_id);
    DWORD mode;
    return GetConsoleMode(handle, &mode) ? handle : NULL;
}

static int __ku_input_write(__ku_input_console console, const char *text, size_t len) {
    if (len == 0) return fflush(stdout) == 0;
    if (console != NULL) {
        if (len > INT_MAX || fflush(stdout) != 0) return 0;
        if (len == 1 && text[0] == '\n') {
            DWORD written;
            return WriteConsoleW(console, L"\r\n", 2, &written, NULL) && written == 2;
        }
        int length = MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, text, (int)len, NULL, 0);
        if (length <= 0) return 0;
        wchar_t *wide = malloc((size_t)length * sizeof(*wide));
        if (wide == NULL) return 0;
        DWORD written;
        int ok = MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, text, (int)len, wide, length)
            && WriteConsoleW(console, wide, (DWORD)length, &written, NULL)
            && written == (DWORD)length;
        free(wide);
        return ok;
    }
    int mode = _setmode(_fileno(stdout), _O_BINARY);
    if (mode == -1) return 0;
    int ok = fwrite(text, 1, len, stdout) == len && fflush(stdout) == 0;
    return _setmode(_fileno(stdout), mode) != -1 && ok;
}

/* Keep cooked UTF-16 input and the console's editor/IME, including Ctrl+C.
 * Cooked reads on consoles without a reliable no-newline submission use
 * one prompt segment per line rather than estimating glyph cell widths. */
static int __ku_input_console_line(__ku_input_console console, KuInputValue *value) {
    wchar_t *wide = NULL;
    size_t len = 0;
    int status = __ku_input_ok;
    for (;;) {
        wchar_t chunk[256];
        DWORD count;
        if (!ReadConsoleW(console, chunk, 256, &count, NULL)) {
            status = __ku_input_io;
            break;
        }
        if (count == 0) {
            status = len == 0 ? __ku_input_eof : __ku_input_ok;
            break;
        }
        if (len == 0 && chunk[0] == 0x1a) {
            status = __ku_input_eof;
            break;
        }
        if (len > INT_MAX - count) {
            status = __ku_input_alloc;
            break;
        }
        void *buffer = realloc(wide, (len + count) * sizeof(*wide));
        if (buffer == NULL) {
            status = __ku_input_alloc;
            break;
        }
        wide = buffer;
        memcpy(wide + len, chunk, count * sizeof(*wide));
        len += count;
        if (wide[len - 1] == L'\n') {
            --len;
            if (len != 0 && wide[len - 1] == L'\r') --len;
            break;
        }
    }
    if (status == __ku_input_ok && len != 0) {
        int length = WideCharToMultiByte(CP_UTF8, WC_ERR_INVALID_CHARS, wide, (int)len, NULL, 0, NULL, NULL);
        if (length <= 0) {
            status = __ku_input_io;
        } else {
            value->bytes = malloc((size_t)length);
            if (value->bytes == NULL) {
                status = __ku_input_alloc;
            } else if (!WideCharToMultiByte(CP_UTF8, WC_ERR_INVALID_CHARS, wide, (int)len,
                                           (char *)value->bytes, length, NULL, NULL)) {
                status = __ku_input_io;
            } else {
                value->len = (size_t)length;
            }
        }
    }
    free(wide);
    return status;
}
#else
typedef int __ku_input_console;

static int __ku_input_write(__ku_input_console console, const char *text, size_t len) {
    (void)console;
    return (len == 0 || fwrite(text, 1, len, stdout) == len) && fflush(stdout) == 0;
}

/* Enter normally sends CR. Canonical VEOF submits it without echoing a
 * delimiter. The tty still owns erase, echo, UTF-8 and natural line wrapping. */
static const int __ku_input_signals[] = {SIGINT, SIGQUIT, SIGTERM, SIGHUP, SIGTSTP};
#define __KU_INPUT_SIGNAL_COUNT (sizeof(__ku_input_signals) / sizeof(__ku_input_signals[0]))
static struct termios __ku_input_original;
static struct sigaction __ku_input_previous[__KU_INPUT_SIGNAL_COUNT];
static volatile sig_atomic_t __ku_input_interrupted;
#if defined(__APPLE__)
static volatile sig_atomic_t __ku_input_reverse_wrap;
static const char __ku_input_wrap_enable[] = "\x1b[?45s\x1b[?45h";
static const char __ku_input_wrap_restore[] = "\x1b[?45l\x1b[?45r";
#endif

static void __ku_input_interrupt(int signal_number) {
    int saved_errno = errno;
    __ku_input_interrupted = 1;
    (void)tcsetattr(STDIN_FILENO, TCSANOW, &__ku_input_original);
#if defined(__APPLE__)
    if (__ku_input_reverse_wrap) {
        (void)write(STDOUT_FILENO, __ku_input_wrap_restore, sizeof(__ku_input_wrap_restore) - 1);
    }
#endif
    for (size_t i = 0; i < __KU_INPUT_SIGNAL_COUNT; ++i) {
        if (__ku_input_signals[i] == signal_number) {
            (void)sigaction(signal_number, &__ku_input_previous[i], NULL);
            (void)kill(getpid(), signal_number);
            break;
        }
    }
    errno = saved_errno;
}

static int __ku_input_terminal_mode(void) {
    const char *term = getenv("TERM");
    if (term == NULL || *term == '\0' || strcmp(term, "dumb") == 0) return 0;
    if (tcgetattr(STDIN_FILENO, &__ku_input_original) != 0
        || (__ku_input_original.c_lflag & (ICANON | ECHO)) != (ICANON | ECHO)) return 0;
    struct termios mode = __ku_input_original;
    mode.c_iflag &= ~(ICRNL | IGNCR | INLCR);
#ifdef ECHOCTL
    mode.c_lflag |= ECHOCTL;
#endif
#ifdef IUTF8
    mode.c_iflag |= IUTF8;
#endif
#ifdef ONLCR
    mode.c_oflag &= ~ONLCR;
#endif
    mode.c_cc[VEOL2] = mode.c_cc[VEOF] == (cc_t)0x0d ? (cc_t)0x04 : mode.c_cc[VEOF];
    mode.c_cc[VEOF] = (cc_t)0x0d;
    __ku_input_interrupted = 0;
    struct sigaction handler = {0};
    handler.sa_handler = __ku_input_interrupt;
    sigfillset(&handler.sa_mask);
    size_t installed = 0;
    while (installed < __KU_INPUT_SIGNAL_COUNT) {
        int sig = __ku_input_signals[installed];
        if (sigaction(sig, NULL, &__ku_input_previous[installed]) != 0
            || sigaction(sig, &handler, NULL) != 0) break;
        ++installed;
    }
    if (installed == __KU_INPUT_SIGNAL_COUNT && !__ku_input_interrupted
        && tcsetattr(STDIN_FILENO, TCSANOW, &mode) == 0) return 1;
    while (installed != 0) {
        --installed;
        (void)sigaction(__ku_input_signals[installed], &__ku_input_previous[installed], NULL);
    }
    return 0;
}

static int __ku_input_restore_terminal(void) {
    int ok = tcsetattr(STDIN_FILENO, TCSANOW, &__ku_input_original) == 0;
#if defined(__APPLE__)
    if (__ku_input_reverse_wrap) {
        if (!__ku_input_write(1, __ku_input_wrap_restore, sizeof(__ku_input_wrap_restore) - 1)) ok = 0;
        __ku_input_reverse_wrap = 0;
    }
#endif
    for (size_t i = 0; i < __KU_INPUT_SIGNAL_COUNT; ++i) {
        (void)sigaction(__ku_input_signals[i], &__ku_input_previous[i], NULL);
    }
    return ok;
}

static int __ku_input_terminal_line(KuInputValue *value) {
    unsigned char bytes[4096];
    if (__ku_input_interrupted) return __ku_input_io;
    ssize_t n = read(STDIN_FILENO, bytes, sizeof(bytes));
    if (n < 0 || __ku_input_interrupted) return __ku_input_io;
#if defined(__APPLE__)
    // Darwin echoes VEOF as a caret sequence, then backspaces over it.
    // Clear those cells in place so they never become part of the answer.
    if (!__ku_input_write(1, "\x1b[K", 3)) return __ku_input_io;
#endif
    if (n == 0) {
        // Empty Enter and a disconnected tty both produce a zero-length read.
        struct pollfd terminal = {STDIN_FILENO, POLLIN, 0};
        if (poll(&terminal, 1, 0) < 0) return __ku_input_io;
        return terminal.revents & (POLLHUP | POLLERR) ? __ku_input_eof : __ku_input_ok;
    }
    size_t len = (size_t)n;
    if (bytes[len - 1] == __ku_input_original.c_cc[VEOF]) {
        if (--len == 0) return __ku_input_eof;
    } else if (bytes[len - 1] == '\n') {
        // Ctrl+J sends LF. With ONLCR off, undo the single row advance
        // without computing a column from the user's UTF-8 bytes.
        if (!__ku_input_write(1, "\x1b[A", 3)) return __ku_input_io;
        --len;
    }
    if (!__ku_input_utf8(bytes, len)) return __ku_input_io;
    if (len != 0) {
        value->bytes = malloc(len);
        if (value->bytes == NULL) return __ku_input_alloc;
        memcpy(value->bytes, bytes, len);
        value->len = len;
    }
    return __ku_input_ok;
}
#endif

KU_INPUT_SCOPE int ku_input_read(
    const char *prompt, size_t prompt_len,
    const char *const *segments, const size_t *segment_lens,
    const uint8_t *field_kinds, size_t count, KuInputValue *fields
) {
    int status = __ku_input_ok;
#ifdef _WIN32
    __ku_input_console in = __ku_input_console_at(STD_INPUT_HANDLE);
    __ku_input_console out = __ku_input_console_at(STD_OUTPUT_HANDLE);
    int interactive = segments != NULL && in != NULL && out != NULL;
    int same_line = 0;
    int stdin_mode = in == NULL ? _setmode(_fileno(stdin), _O_BINARY) : 0;
    if (stdin_mode == -1) return __ku_input_io;
#else
    int out = isatty(STDOUT_FILENO);
    int interactive = segments != NULL && isatty(STDIN_FILENO) && out;
    int same_line = interactive && __ku_input_terminal_mode();
#if defined(__APPLE__)
    if (same_line) {
        // Darwin echoes the canonical submit as ^M and sends two backspaces.
        // Reverse wrapping lets those backspaces undo the echo across a soft
        // line break. Reset for terminals without private-mode save/restore.
        __ku_input_reverse_wrap = 1;
        if (!__ku_input_write(out, __ku_input_wrap_enable, sizeof(__ku_input_wrap_enable) - 1)) {
            status = __ku_input_io;
        }
    }
#endif
#endif
    if (segments == NULL && !__ku_input_write(out, prompt, prompt_len)) {
        status = __ku_input_io;
    }
    for (size_t i = 0; status == __ku_input_ok && i < count; ++i) {
        do {
            if (interactive && !__ku_input_write(out, segments[i], segment_lens[i])) {
                status = __ku_input_io;
                break;
            }
#ifdef _WIN32
            status = in != NULL ? __ku_input_console_line(in, &fields[i])
                                : __ku_input_stream_line(&fields[i]);
#else
            status = same_line ? __ku_input_terminal_line(&fields[i])
                               : __ku_input_stream_line(&fields[i]);
#endif
            if (status == __ku_input_ok && field_kinds != NULL && field_kinds[i] == 1) {
                status = __ku_input_integer(&fields[i]);
            }
            if (status != __ku_input_invalid_int || !interactive) break;
            ku_input_free(&fields[i], 1);
            const char *message = "Invalid integer. Enter a signed 64-bit decimal.";
            if ((same_line && !__ku_input_write(out, "\r\n", 2))
                || !__ku_input_write(out, message, strlen(message))
                || !__ku_input_write(out, same_line ? "\r\n" : "\n", same_line ? 2 : 1)) {
                status = __ku_input_io;
                break;
            }
        } while (status == __ku_input_invalid_int);
    }
#ifndef _WIN32
    // Restore output CR/LF handling before writing the final newline.
    if (same_line && !__ku_input_restore_terminal()) status = __ku_input_io;
#else
    if (in == NULL && _setmode(_fileno(stdin), stdin_mode) == -1) status = __ku_input_io;
#endif
    if (interactive && status == __ku_input_ok) {
        if (!__ku_input_write(out, segments[count], segment_lens[count])
            || ((same_line || segment_lens[count] != 0) && !__ku_input_write(out, "\n", 1))) {
            status = __ku_input_io;
        }
    }
    if (status != __ku_input_ok) ku_input_free(fields, count);
    return status;
}
