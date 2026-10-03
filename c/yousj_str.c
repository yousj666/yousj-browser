/* yousj_str.c — Yousj browser engine: low-level string utilities (C layer). */
#include "yousj_str.h"
#include <string.h>
#include <stdint.h>

static int ascii_lower(int c)
{
    return (c >= 'A' && c <= 'Z') ? c + ('a' - 'A') : c;
}

int yousj_tag_eq(const char *a, size_t a_len, const char *b)
{
    size_t i = 0;
    while (i < a_len && b[i] != '\0') {
        if (ascii_lower((unsigned char)a[i]) != ascii_lower((unsigned char)b[i]))
            return 0;
        i++;
    }
    return (i == a_len && b[i] == '\0') ? 1 : 0;
}

int yousj_is_void_tag(const char *name, size_t len)
{
    static const char *voids[] = {
        "area", "base", "br", "col", "embed", "hr", "img", "input",
        "link", "meta", "param", "source", "track", "wbr", NULL
    };
    for (int i = 0; voids[i] != NULL; i++) {
        if (yousj_tag_eq(name, len, voids[i]))
            return 1;
    }
    return 0;
}

typedef struct {
    const char *name;
    uint32_t cp;
} YousjEnt;

static const YousjEnt kEnts[] = {
    { "amp", 38 },   { "lt", 60 },    { "gt", 62 },
    { "quot", 34 },  { "apos", 39 },  { "nbsp", 160 },
    { "copy", 169 }, { "reg", 174 },  { "hellip", 8230 },
    { "mdash", 8212 },{ "ndash", 8211 },{ "lsquo", 8216 },
    { "rsquo", 8217 },{ "ldquo", 8220 },{ "rdquo", 8221 },
    { "laquo", 171 },{ "raquo", 187 }, { "times", 215 },
    { "divide", 247 },{ "sect", 167 }, { "para", 182 },
    { NULL, 0 }
};

static size_t utf8_encode(uint32_t cp, char *out)
{
    if (cp < 0x80) {
        out[0] = (char)cp;
        return 1;
    }
    if (cp < 0x800) {
        out[0] = (char)(0xC0 | (cp >> 6));
        out[1] = (char)(0x80 | (cp & 0x3F));
        return 2;
    }
    if (cp < 0x10000) {
        out[0] = (char)(0xE0 | (cp >> 12));
        out[1] = (char)(0x80 | ((cp >> 6) & 0x3F));
        out[2] = (char)(0x80 | (cp & 0x3F));
        return 3;
    }
    out[0] = (char)(0xF0 | (cp >> 18));
    out[1] = (char)(0x80 | ((cp >> 12) & 0x3F));
    out[2] = (char)(0x80 | ((cp >> 6) & 0x3F));
    out[3] = (char)(0x80 | (cp & 0x3F));
    return 4;
}

size_t yousj_decode_entity(const char *name, size_t name_len,
                           char *out, size_t out_cap)
{
    uint32_t cp = 0;
    int found = 0;

    if (name_len > 1 && name[0] == '#') {
        if (name[1] == 'x' || name[1] == 'X') {
            for (size_t i = 2; i < name_len; i++) {
                char c = name[i];
                uint32_t v;
                if (c >= '0' && c <= '9') v = (uint32_t)(c - '0');
                else if (c >= 'a' && c <= 'f') v = (uint32_t)(c - 'a' + 10);
                else if (c >= 'A' && c <= 'F') v = (uint32_t)(c - 'A' + 10);
                else return 0;
                cp = cp * 16 + v;
            }
        } else {
            for (size_t i = 1; i < name_len; i++) {
                if (name[i] < '0' || name[i] > '9')
                    return 0;
                cp = cp * 10 + (uint32_t)(name[i] - '0');
            }
        }
        found = 1;
    } else {
        for (int i = 0; kEnts[i].name != NULL; i++) {
            if (yousj_tag_eq(name, name_len, kEnts[i].name)) {
                cp = kEnts[i].cp;
                found = 1;
                break;
            }
        }
    }

    if (!found || cp == 0 || cp > 0x10FFFF)
        return 0;

    char tmp[4];
    size_t n = utf8_encode(cp, tmp);
    if (n > out_cap)
        return 0;
    memcpy(out, tmp, n);
    return n;
}
