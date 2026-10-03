/* yousj_str.h — Yousj browser engine: low-level string utilities (C layer).
 * Hot-path helpers for the HTML tokenizer: ASCII case-insensitive
 * tag comparison, void-element lookup, and character entity decoding. */
#ifndef YOUSJ_STR_H
#define YOUSJ_STR_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ASCII case-insensitive compare: a (a_len bytes) vs NUL-terminated b.
 * Returns 1 if equal, 0 otherwise. */
int yousj_tag_eq(const char *a, size_t a_len, const char *b);

/* Returns 1 if the tag name is an HTML void element. */
int yousj_is_void_tag(const char *name, size_t len);

/* Decode one entity body (without '&' and ';'), e.g. "amp", "#65", "#x41".
 * Writes UTF-8 to out (out_cap bytes), returns bytes written (0 = unknown). */
size_t yousj_decode_entity(const char *name, size_t name_len,
                           char *out, size_t out_cap);

#ifdef __cplusplus
}
#endif

#endif /* YOUSJ_STR_H */
