/* fast-anything recorder runtime for C: compiled into the app for one run. */
#ifndef _CRT_SECURE_NO_WARNINGS
#define _CRT_SECURE_NO_WARNINGS
#endif
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "fa_clock.h"
#include "fa_rec.h"
#ifndef _WIN32
#include <errno.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <unistd.h>
#endif

int fa_rec_inside;
const int fa_rec_slices[10] = {0, 1, 2, 3, 5, 8, 13, 21, 34, 55};

typedef struct { char *p; size_t len, cap; } fa_buf;
static fa_buf fa_args, fa_line;
static char **fa_kept, **fa_derived;
static size_t fa_nkept, fa_nderived, fa_limit;
static long long fa_calls, fa_skipped, fa_nanos, fa_derive_started, fa_edge_started;
static size_t fa_nedges, fa_edge_calls;
static unsigned fa_grown;
static int fa_bad; /* the current call has something JSON can't hold */
static unsigned long long fa_rng = 0xFA57;

static void fa_put(fa_buf *b, const char *s, size_t n) {
    if (b->len + n + 1 > b->cap) {
        b->cap = (b->len + n + 1) * 2;
        b->p = (char *) realloc(b->p, b->cap);
    }
    memcpy(b->p + b->len, s, n);
    b->len += n;
    b->p[b->len] = 0;
}
static void fa_puts(fa_buf *b, const char *s) { fa_put(b, s, strlen(s)); }
static void fa_sep(void) {
    if (fa_args.len > 1) fa_puts(&fa_args, ",");
}

long long fa_rec_now(void) { return fa_now(); }
void fa_rec_begin(void) {
    fa_args.len = 0;
    fa_bad = 0;
    fa_puts(&fa_args, "[");
}
void fa_rec_i64(long long v) {
    char t[32];
    fa_sep();
    snprintf(t, sizeof t, "%lld", v);
    fa_puts(&fa_args, t);
}
void fa_rec_u64(unsigned long long v) {
    char t[32];
    fa_sep();
    snprintf(t, sizeof t, "%llu", v);
    fa_puts(&fa_args, t);
}
static void fa_f64_to(fa_buf *b, double v) {
    char t[40];
    if (!isfinite(v)) {
        fa_bad = 1;
        fa_puts(b, "null");
        return;
    }
    snprintf(t, sizeof t, "%.17g", v);
    if (!strpbrk(t, ".eE")) strcat(t, ".0");
    fa_puts(b, t);
}
void fa_rec_f64(double v) {
    fa_sep();
    fa_f64_to(&fa_args, v);
}
void fa_rec_bool(int v) {
    fa_sep();
    fa_puts(&fa_args, v ? "true" : "false");
}
static int fa_utf8_ok(const unsigned char *s) {
    while (*s) {
        int n = *s < 0x80 ? 0 : (*s >> 5) == 6 ? 1 : (*s >> 4) == 14 ? 2 : (*s >> 3) == 30 ? 3 : -1;
        if (n < 0) return 0;
        s++;
        while (n--) {
            if ((*s & 0xC0) != 0x80) return 0;
            s++;
        }
    }
    return 1;
}
void fa_rec_str(const char *s) {
    fa_sep();
    if (!s) {
        fa_puts(&fa_args, "null");
        return;
    }
    if (!fa_utf8_ok((const unsigned char *) s)) fa_bad = 1;
    fa_puts(&fa_args, "\"");
    for (const unsigned char *c = (const unsigned char *) s; *c; c++) {
        char t[8];
        if (*c == '"' || *c == '\\') {
            t[0] = '\\';
            t[1] = (char) *c;
            fa_put(&fa_args, t, 2);
        } else if (*c < 0x20) {
            snprintf(t, sizeof t, "\\u%04x", *c);
            fa_puts(&fa_args, t);
        } else {
            fa_put(&fa_args, (const char *) c, 1);
        }
    }
    fa_puts(&fa_args, "\"");
}
void fa_rec_arr_i64(const void *p, size_t n, int size, int is_signed) {
    fa_sep();
    fa_puts(&fa_args, "[");
    for (size_t i = 0; i < n; i++) {
        char t[32];
        const char *e = (const char *) p + i * size;
        if (is_signed) {
            long long v = size == 1 ? *(const signed char *) e : size == 2 ? *(const short *) e : size == 4 ? *(const int *) e : *(const long long *) e;
            snprintf(t, sizeof t, "%s%lld", i ? "," : "", v);
        } else {
            unsigned long long v = size == 1 ? *(const unsigned char *) e : size == 2 ? *(const unsigned short *) e : size == 4 ? *(const unsigned *) e : *(const unsigned long long *) e;
            snprintf(t, sizeof t, "%s%llu", i ? "," : "", v);
        }
        fa_puts(&fa_args, t);
    }
    fa_puts(&fa_args, "]");
}
void fa_rec_arr_f64(const void *p, size_t n, int size) {
    fa_sep();
    fa_puts(&fa_args, "[");
    for (size_t i = 0; i < n; i++) {
        if (i) fa_puts(&fa_args, ",");
        fa_f64_to(&fa_args, size == 4 ? (double) ((const float *) p)[i] : ((const double *) p)[i]);
    }
    fa_puts(&fa_args, "]");
}

static unsigned long long fa_next(void) {
    fa_rng ^= fa_rng << 13;
    fa_rng ^= fa_rng >> 7;
    fa_rng ^= fa_rng << 17;
    return fa_rng;
}
size_t fa_rec_rand(size_t below) { return below ? (size_t) (fa_next() % below) : 0; }

/* derived: NULL for a recorded call, else "true" or "\"edge\"" */
static char *fa_finish(const char *result, const char *derived) {
    fa_line.len = 0;
    fa_puts(&fa_line, "{\"args\":");
    fa_puts(&fa_line, fa_args.p);
    fa_puts(&fa_line, "],\"result\":");
    fa_puts(&fa_line, result);
    if (derived) {
        fa_puts(&fa_line, ",\"derived\":");
        fa_puts(&fa_line, derived);
    }
    fa_puts(&fa_line, "}");
    char *copy = (char *) malloc(fa_line.len + 1);
    memcpy(copy, fa_line.p, fa_line.len + 1);
    return copy;
}
static void fa_keep(const char *result, long long took) {
    if (!fa_limit) {
        const char *l = getenv("FAST_ANYTHING_RECORD_LIMIT");
        fa_limit = l && atoi(l) > 0 ? (size_t) atoi(l) : 500;
        fa_kept = (char **) calloc(fa_limit, sizeof(char *));
    }
    fa_calls++;
    fa_nanos += took;
    if (fa_bad) {
        fa_skipped++;
        return;
    }
    size_t slot = fa_nkept < fa_limit ? fa_nkept : (size_t) (fa_next() % (unsigned long long) fa_calls);
    if (slot >= fa_limit) return;
    char *line = fa_finish(result, NULL);
    if (slot < fa_nkept) {
        free(fa_kept[slot]);
    } else {
        fa_nkept++;
    }
    fa_kept[slot] = line;
}
static void fa_derive(const char *result, const char *tag) {
    if (fa_bad) return;
    fa_derived = (char **) realloc(fa_derived, (fa_nderived + 1) * sizeof(char *));
    fa_derived[fa_nderived++] = fa_finish(result, tag);
}
static const char *fa_fmt_i64(long long r) { static char t[32]; snprintf(t, sizeof t, "%lld", r); return t; }
static const char *fa_fmt_u64(unsigned long long r) { static char t[32]; snprintf(t, sizeof t, "%llu", r); return t; }
static const char *fa_fmt_f64(double r) {
    static fa_buf b;
    b.len = 0;
    fa_f64_to(&b, r);
    return b.p;
}
void fa_rec_done_i64(long long r, long long took) { fa_keep(fa_fmt_i64(r), took); }
void fa_rec_done_u64(unsigned long long r, long long took) { fa_keep(fa_fmt_u64(r), took); }
void fa_rec_done_f64(double r, long long took) { fa_keep(fa_fmt_f64(r), took); }
void fa_rec_done_bool(int r, long long took) { fa_keep(r ? "true" : "false", took); }

static int fa_derive_on(void) {
    const char *d = getenv("FAST_ANYTHING_RECORD_DERIVE");
    return !(d && d[0] == '0');
}
int fa_rec_derive_ok(void) {
    if (!fa_derive_on()) return 0;
    if (!fa_derive_started) fa_derive_started = fa_now();
    return fa_nderived < 200 + fa_nedges && fa_now() - fa_derive_started < 3000000000LL;
}

/* Edge cases: inputs where ports and originals tend to part ways (special numbers, non-ASCII text,
 * empty and much bigger inputs), from the first 20 recorded calls, within 1.5s and 100 cases.
 * A C function can't throw: an input outside its domain crashes it, or never returns. So the
 * original answers each one in a child process, and a crash or hang only skips that case. Where
 * there is no fork (Windows), there are no edge cases. */
long long fa_rec_edge_took = -1;
int fa_rec_edge_call(void) {
#ifdef _WIN32
    return 0;
#else
    if (!fa_derive_on() || fa_edge_calls >= 20) return 0;
    fa_edge_calls++;
    if (!fa_edge_started) fa_edge_started = fa_now();
    return 1;
#endif
}
long long fa_rec_edge_left(void) { return 1500000000LL - (fa_now() - fa_edge_started); }
int fa_rec_edge_ok(void) { return fa_nedges < 100 && fa_rec_edge_left() > 0; }
int fa_rec_grow(int param) {
    unsigned bit = 1u << (param & 31);
    if (fa_grown & bit) return 0;
    fa_grown |= bit;
    return 1;
}

#ifndef _WIN32
static int fa_edge_out = -1;
static char **fa_edge_seen; /* every edge input tried, so one that crashes or hangs isn't tried again */
static size_t fa_nseen;
/* Runs the call (the code after it) in a child process; returns 1 there, 0 here. Here, it sets
 * fa_rec_edge_took (-1 if the child crashed, hung or gave a value JSON can't hold) and records
 * the case with the child's result: kind 1 an edge case, 2 a derived one, 0 none (only timed). */
int fa_rec_fork(int kind) {
    fa_rec_edge_took = -1;
    if (fa_bad || (kind != 2 && !fa_rec_edge_ok())) return 0;
    if (kind == 1) {
        for (size_t i = 0; i < fa_nseen; i++)
            if (!strcmp(fa_edge_seen[i], fa_args.p)) return 0;
        fa_edge_seen = (char **) realloc(fa_edge_seen, (fa_nseen + 1) * sizeof(char *));
        fa_edge_seen[fa_nseen] = (char *) malloc(fa_args.len + 1);
        memcpy(fa_edge_seen[fa_nseen++], fa_args.p, fa_args.len + 1);
    }
    int fd[2];
    if (pipe(fd)) return 0;
    fflush(NULL);
    long long t = fa_now();
    pid_t pid = fork();
    if (pid == 0) {
        close(fd[0]);
        fa_edge_out = fd[1];
        /* a case that never returns ends after 250ms (or what's left of the edge budget) */
        long long left = kind == 2 ? 250000 : fa_rec_edge_left() / 1000;
        struct itimerval timer = {{0, 0}, {0, (long) (left < 250000 ? (left > 1000 ? left : 1000) : 250000)}};
        setitimer(ITIMER_REAL, &timer, NULL);
        return 1;
    }
    close(fd[1]);
    if (pid < 0) {
        close(fd[0]);
        return 0;
    }
    fa_buf got = {0, 0, 0};
    char chunk[256];
    ssize_t n;
    int read_ok = 1;
    while ((n = read(fd[0], chunk, sizeof chunk)) != 0) {
        if (n > 0) {
            fa_put(&got, chunk, (size_t) n);
        } else if (errno != EINTR) {
            read_ok = 0;
            break;
        }
    }
    close(fd[0]);
    int status = 0;
    while (waitpid(pid, &status, 0) < 0) {
        if (errno != EINTR) {
            free(got.p);
            return 0;
        }
    }
    long long took = fa_now() - t;
    if (read_ok && WIFEXITED(status) && WEXITSTATUS(status) == 0 && got.len) {
        fa_rec_edge_took = took;
        if (kind == 1) fa_nedges++;
        if (kind) fa_derive(got.p, kind == 1 ? "\"edge\"" : "true");
    }
    free(got.p);
    return 0;
}
static void fa_answer(const char *result) {
    size_t len = strlen(result);
    int ok = !fa_bad && write(fa_edge_out, result, len) == (ssize_t) len;
    _exit(ok ? 0 : 1);
}
const void *fa_rec_empty(const void *p) {
    (void) p;
    return NULL;
}
#else
/* no fork: derived cases run here, as the call they come from did */
int fa_rec_fork(int kind) { return kind == 2; }
static void fa_answer(const char *result) { fa_derive(result, "true"); }
const void *fa_rec_empty(const void *p) { return p; }
#endif
void fa_rec_answer_i64(long long r) { fa_answer(fa_fmt_i64(r)); }
void fa_rec_answer_u64(unsigned long long r) { fa_answer(fa_fmt_u64(r)); }
void fa_rec_answer_f64(double r) { fa_answer(fa_fmt_f64(r)); }
void fa_rec_answer_bool(int r) { fa_answer(r ? "true" : "false"); }

static char *fa_dup_str(const char *a, const char *b, const char *c) {
    size_t la = strlen(a), lb = strlen(b), lc = strlen(c);
    char *out = (char *) malloc(la + lb + lc + 1);
    memcpy(out, a, la);
    memcpy(out + la, b, lb);
    memcpy(out + la + lb, c, lc + 1);
    return out;
}
/* Text edge k (0-4): an accent in the middle, CJK at the end, an emoji with a modifier at the
 * start, a combining mark at the end, empty. NULL when it's the text itself. */
char *fa_rec_str_edge(const char *s, int k) {
    const char *a = "\xC3\xA9", *cjk = "\xE6\x97\xA5\xE6\x9C\xAC", *emoji = "\xF0\x9F\x91\x8D\xF0\x9F\x8F\xBD", *comb = "e\xCC\x81";
    if (k == 0) {
        char *head = fa_rec_substr(s, 0, fa_rec_chars(s) / 2);
        char *out = fa_dup_str(head, a, s + strlen(head));
        free(head);
        return out;
    }
    if (k == 1) return fa_dup_str(s, cjk, "");
    if (k == 2) return fa_dup_str(emoji, s, "");
    if (k == 3) return fa_dup_str(s, comb, "");
    return *s ? fa_dup_str("", "", "") : NULL;
}
/* s repeated factor times; NULL past 4096 characters */
char *fa_rec_str_repeat(const char *s, int factor) {
    size_t chars = fa_rec_chars(s), len = strlen(s);
    if (!chars || chars * (size_t) factor > 4096) return NULL;
    char *out = (char *) malloc(len * (size_t) factor + 1);
    for (int k = 0; k < factor; k++) memcpy(out + len * (size_t) k, s, len);
    out[len * (size_t) factor] = 0;
    return out;
}
/* n items of size bytes repeated factor times; NULL past 4096 items */
void *fa_rec_repeat(const void *p, size_t n, size_t size, int factor) {
    if (!n || n * (size_t) factor > 4096) return NULL;
    char *out = (char *) malloc(n * size * (size_t) factor);
    for (int k = 0; k < factor; k++) memcpy(out + n * size * (size_t) k, p, n * size);
    return out;
}
void *fa_rec_dup(const void *p, size_t n, size_t size) {
    void *out = malloc(n * size + 1);
    memcpy(out, p, n * size);
    return out;
}

size_t fa_rec_chars(const char *s) {
    size_t n = 0;
    for (; *s; s++) n += ((unsigned char) *s & 0xC0) != 0x80;
    return n;
}
char *fa_rec_substr(const char *s, size_t off, size_t len) {
    const char *a = s;
    while (*a && off) {
        a++;
        while (((unsigned char) *a & 0xC0) == 0x80) a++;
        off--;
    }
    const char *b = a;
    while (*b && len) {
        b++;
        while (((unsigned char) *b & 0xC0) == 0x80) b++;
        len--;
    }
    char *out = (char *) malloc((size_t) (b - a) + 1);
    memcpy(out, a, (size_t) (b - a));
    out[b - a] = 0;
    return out;
}
void fa_rec_free(void *p) { free(p); }

static void fa_rec_save(void) {
    const char *path = getenv("FAST_ANYTHING_RECORD_OUT"), *meta = getenv("FAST_ANYTHING_RECORD_META");
    FILE *out = path ? fopen(path, "wb") : NULL;
    if (out) {
        for (size_t i = 0; i < fa_nkept; i++) fprintf(out, "%s\n", fa_kept[i]);
        for (size_t i = 0; i < fa_nderived; i++) fprintf(out, "%s\n", fa_derived[i]);
        fclose(out);
    }
    FILE *m = meta ? fopen(meta, "wb") : NULL;
    if (m) {
        fprintf(m, "{\"calls\":%lld,\"kept\":%zu,\"skipped\":%lld,\"unsupported\":[%s],\"original_ns\":%.3f}", fa_calls, fa_nkept,
                fa_skipped, fa_skipped ? "\"NaN, infinity or a string that isn't UTF-8\"" : "", fa_calls ? (double) fa_nanos / (double) fa_calls : 0.0);
        fclose(m);
    }
}

__attribute__((constructor)) static void fa_rec_start(void) { atexit(fa_rec_save); }
