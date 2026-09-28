/* AINL AOT micro-runtime.
 *
 * Inlined into every generated .c file so the compiled binary is standalone
 * (zero runtime deps beyond libc). Faithfully ports the interpreter's Value
 * model (nil/bool/int/float/str/sym/list/map/builtin/closure), lexical scopes
 * with a parent chain (closures capture their defining scope), cons-cell
 * lists, and the numeric model (i64 with promotion to f64 on overflow, per
 * docs/NUMERIC_MODEL.md).
 *
 * The codegen (ainl-cc) emits compiled functions that call into this runtime.
 * Local variables are dense slots (a Value array); captured (non-local)
 * variables are resolved through the scope chain.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <stdarg.h>
#include <math.h>
/* Stage 3.1 stdlib: time(), getenv() and file I/O. time()/clock() are in libc
 * on POSIX; the C23 additions (timespec_get) are avoided for portability. */
#include <time.h>

/* ---- error handling ---------------------------------------------------- */
static int g_err = 0;
static char g_errmsg[1024];
static void set_err(const char *fmt, ...);

/* ---- step counter (bounds runaway loops / recursion) ------------------- */
static uint64_t g_steps = 0;
static uint64_t g_max_steps = 2000000;
static void steps_init(void) {
  const char *e = getenv("AINL_MAX_STEPS");
  if (e) {
    long long v = atoll(e);
    if (v > 0) g_max_steps = (uint64_t)v;
  }
}
/* Bump the step budget. On exhaustion, set g_err (the generated code checks
 * g_err after each while-iteration and at the end of main) so runaway loops
 * and recursion are bounded and reported, matching the interpreter's safety
 * property. Granularity: one tick per while-iteration and per function call
 * (the two runaway vectors), rather than per node eval — this bounds runaway
 * work without adding per-expression overhead to the hot loop. */
static void tick(void) {
  g_steps++;
  if (g_steps > g_max_steps) {
    set_err(
        "step limit exceeded (max %llu evaluation steps) — likely an "
        "infinite loop or runaway recursion",
        (unsigned long long)g_max_steps);
  }
}

/* ---- error handling ---------------------------------------------------- */
static void set_err(const char *fmt, ...) {
  va_list ap;
  va_start(ap, fmt);
  vsnprintf(g_errmsg, sizeof(g_errmsg), fmt, ap);
  va_end(ap);
  g_err = 1;
}

/* ---- value model ------------------------------------------------------- */
typedef enum {
  V_NIL, V_BOOL, V_INT, V_FLOAT, V_STR, V_SYM, V_LIST, V_MAP, V_BUILTIN,
  V_CLOSURE
} VTag;

typedef struct Value Value;
typedef struct Str Str;
typedef struct ConsCell ConsCell;
typedef struct Map Map;
typedef struct Closure Closure;
typedef struct Scope Scope;

struct Value {
  VTag tag;
  union {
    int b;
    int64_t i;
    double f;
    Str *s;
    ConsCell *l;
    Map *m;
    Closure *c;
    int builtin;
  } u;
};

struct Str {
  int ref;
  char *data;
  size_t len;
};
struct ConsCell {
  int ref;
  Value head;
  ConsCell *tail;
  int len;
};
struct Map {
  int ref;
  int n;
  Value *keys;
  Value *vals;
};
struct Scope {
  int ref;
  Scope *parent;
  int nvars;
  char **names;
  Value *vals;
};

enum {
  B_ADD, B_MUL, B_SUB, B_DIV, B_EQ, B_LT, B_GT, B_LE, B_GE, B_NOT, B_MOD,
  B_PRINT, B_STR, B_LIST, B_LEN, B_FIRST, B_REST, B_NTH, B_CONS, B_PUSH,
  B_HASH, B_GET, B_ASSOC, B_HAS, B_KEYS, B_VALS, B_ERROR,
  /* Stage 3.1 stdlib — the ids here must match BUILTIN_IDS in ainl-cc's
   * lib.rs exactly; the AOT correctness suite fails loudly if they drift. */
  B_READ_FILE, B_WRITE_FILE, B_APPEND_FILE,
  B_SPLIT, B_JOIN, B_TRIM, B_REPLACE, B_UPCASE, B_DOWNCASE, B_CONTAINS,
  B_ENV_GET, B_EXIT,
  B_NOW, B_SLEEP,
  B_ABS, B_MIN, B_MAX, B_FLOOR, B_SQRT,
  /* Tier 1 file I/O. Appended at the end rather than grouped with the other
   * file builtins so every Stage 3.1 id keeps the value it has always had —
   * the three tables (this enum, BUILTIN_IDS in ainl-cc's lib.rs, and the
   * runtime's name table) are pinned against each other by
   * crates/ainl-cc/tests/aot_stdlib.rs. */
  B_FILE_EXISTS, B_DELETE_FILE, B_LIST_DIR,
  B_PATH_JOIN, B_PATH_BASE, B_PATH_DIR,
  B_COUNT
};

struct Closure {
  int ref;
  int nparams;
  char **params;
  char *variadic;
  Value (*fn)(Scope *env, Value *args, int nargs);
  Scope *def_env;
};

/* ---- refcounting ------------------------------------------------------- */
static void scope_unref(Scope *s);

static void v_ref(Value *v) {
  switch (v->tag) {
  case V_STR:
    v->u.s->ref++;
    break;
  case V_SYM:
    /* Interned symbols are immortal (pinned ref); no-op. */
    break;
  case V_LIST:
    v->u.l->ref++;
    break;
  case V_MAP:
    v->u.m->ref++;
    break;
  case V_CLOSURE:
    v->u.c->ref++;
    break;
  default:
    break;
  }
}

static void cons_free(ConsCell *c);

static void v_unref(Value *v) {
  switch (v->tag) {
  case V_STR:
    if (--v->u.s->ref == 0) {
      free(v->u.s->data);
      free(v->u.s);
    }
    break;
  case V_SYM:
    /* Interned symbols are immortal (pinned ref); never freed here. */
    break;
  case V_LIST:
    if (--v->u.l->ref == 0)
      cons_free(v->u.l);
    break;
  case V_MAP: {
    Map *m = v->u.m;
    if (--m->ref == 0) {
      for (int i = 0; i < m->n; i++) {
        v_unref(&m->keys[i]);
        v_unref(&m->vals[i]);
      }
      free(m->keys);
      free(m->vals);
      free(m);
    }
    break;
  }
  case V_CLOSURE: {
    Closure *c = v->u.c;
    if (--c->ref == 0) {
      for (int i = 0; i < c->nparams; i++)
        free(c->params[i]);
      free(c->params);
      if (c->variadic)
        free(c->variadic);
      scope_unref(c->def_env);
      free(c);
    }
    break;
  }
  default:
    break;
  }
}

/* Free a cons chain iteratively (a derived recursive free would overflow the
 * stack on long lists). Stops when a shared (still-referenced) tail is hit. */
static void cons_free(ConsCell *c) {
  while (c) {
    ConsCell *tail = c->tail;
    Value head = c->head;
    free(c);
    c = NULL;
    v_unref(&head);
    if (tail) {
      tail->ref--;
      if (tail->ref == 0)
        c = tail;
      else
        break;
    }
  }
}

/* ---- value constructors ------------------------------------------------ */
static Value v_nil(void) {
  Value v;
  v.tag = V_NIL;
  return v;
}
static Value v_bool(int b) {
  Value v;
  v.tag = V_BOOL;
  v.u.b = b;
  return v;
}
static Value v_int(int64_t i) {
  Value v;
  v.tag = V_INT;
  v.u.i = i;
  return v;
}
static Value v_float(double f) {
  Value v;
  v.tag = V_FLOAT;
  v.u.f = f;
  return v;
}
static Value v_str(const char *s) {
  Value v;
  v.tag = V_STR;
  Str *st = malloc(sizeof(Str));
  st->ref = 1;
  st->len = strlen(s);
  st->data = malloc(st->len + 1);
  memcpy(st->data, s, st->len + 1);
  v.u.s = st;
  return v;
}
/* Takes ownership of a malloc'd string. */
static Value v_str_take(char *s) {
  Value v;
  v.tag = V_STR;
  Str *st = malloc(sizeof(Str));
  st->ref = 1;
  st->len = strlen(s);
  st->data = s;
  v.u.s = st;
  return v;
}
static Str *sym_intern(const char *s);
static Value v_sym(const char *s) {
  Value v;
  v.tag = V_SYM;
  v.u.s = sym_intern(s);
  return v;
}
/* ---- symbol interning -------------------------------------------------- */
/* Symbols are interned: identical names share one Str, so a V_SYM is a
 * (tag, Str*) pair that can be compared by pointer equality. This is a
 * standard optimization (Lisp/JS both intern symbols) and changes no
 * semantics: equality, display, and hashing are all by name, which is
 * preserved. The table is never freed (process-lifetime), so interned
 * Strs must not be freed by v_unref — v_ref/v_unref become no-ops for
 * interned symbols (ref is pinned at 1). */
#define SYM_TABLE_CAP 4096
static Str *g_sym_table[SYM_TABLE_CAP];
static int g_sym_count = 0;

static uint32_t sym_hash(const char *s) {
  uint32_t h = 2166136261u;
  while (*s) {
    h ^= (unsigned char)*s++;
    h *= 16777619u;
  }
  return h;
}

/* Returns an interned Str* (ref pinned at 1). */
static Str *sym_intern(const char *s) {
  uint32_t h = sym_hash(s);
  size_t len = strlen(s);
  uint32_t idx = h & (SYM_TABLE_CAP - 1);
  for (uint32_t probe = 0; probe < SYM_TABLE_CAP; probe++) {
    uint32_t slot = (idx + probe) & (SYM_TABLE_CAP - 1);
    Str *e = g_sym_table[slot];
    if (e == NULL) {
      Str *st = malloc(sizeof(Str));
      st->ref = 1; /* pinned; v_unref must not free */
      st->len = len;
      st->data = malloc(len + 1);
      memcpy(st->data, s, len + 1);
      g_sym_table[slot] = st;
      g_sym_count++;
      return st;
    }
    if (e->len == len && memcmp(e->data, s, len) == 0)
      return e;
  }
  /* Table full: fall back to a non-interned Str (still correct, just not
   * shared). */
  Str *st = malloc(sizeof(Str));
  st->ref = 1;
  st->len = len;
  st->data = malloc(len + 1);
  memcpy(st->data, s, len + 1);
  return st;
}

/* Construct a builtin value (no refcount). */
static Value v_builtin(int id) {
  Value v;
  v.tag = V_BUILTIN;
  v.u.builtin = id;
  return v;
}

/* cons cell: takes ownership of `head`'s ref, refs `tail`. */
static ConsCell *cons_cell_new(Value head, ConsCell *tail) {
  ConsCell *c = malloc(sizeof(ConsCell));
  c->ref = 1;
  c->head = head;
  c->tail = tail;
  if (tail)
    tail->ref++;
  c->len = tail ? tail->len + 1 : 1;
  return c;
}

static Value v_list_empty(void) {
  static ConsCell empty_cell = {.ref = 1000000000,
                                .head = {.tag = V_NIL},
                                .tail = NULL,
                                .len = 0};
  Value r;
  r.tag = V_LIST;
  r.u.l = &empty_cell;
  return r;
}
/* Build a closure. Takes ownership of `params` (array of nparams strdup'd
 * names) and `variadic` (strdup'd name or NULL), and refs `def_env`. */
static Value v_closure(int nparams, char **params, char *variadic,
                       Value (*fn)(Scope *, Value *, int), Scope *def_env) {
  Closure *c = malloc(sizeof(Closure));
  c->ref = 1;
  c->nparams = nparams;
  c->params = params;
  c->variadic = variadic;
  c->fn = fn;
  c->def_env = def_env;
  if (def_env)
    def_env->ref++;
  Value r;
  r.tag = V_CLOSURE;
  r.u.c = c;
  return r;
}

/* Build a list from an array; takes ownership of the array's refs. */
static Value v_list_from_array(Value *items, int n) {
  ConsCell *tail = NULL;
  for (int k = n - 1; k >= 0; k--) {
    tail = cons_cell_new(items[k], tail);
  }
  if (tail) {
    Value r;
    r.tag = V_LIST;
    r.u.l = tail;
    return r;
  }
  return v_list_empty();
}

/* ---- scopes ------------------------------------------------------------ */
static Scope *scope_new(Scope *parent) {
  Scope *s = calloc(1, sizeof(Scope));
  s->ref = 1;
  s->parent = parent;
  if (parent)
    parent->ref++;
  return s;
}
static void scope_ref(Scope *s) {
  if (s)
    s->ref++;
}
static void scope_unref(Scope *s) {
  if (!s)
    return;
  if (--s->ref == 0) {
    for (int i = 0; i < s->nvars; i++) {
      free(s->names[i]);
      v_unref(&s->vals[i]);
    }
    free(s->names);
    free(s->vals);
    if (s->parent)
      scope_unref(s->parent);
    free(s);
  }
}
/* Takes ownership of one ref of `val`. Updates in place if the name exists. */
static void scope_define(Scope *s, const char *name, Value val) {
  for (int i = 0; i < s->nvars; i++) {
    if (strcmp(s->names[i], name) == 0) {
      v_unref(&s->vals[i]);
      s->vals[i] = val;
      return;
    }
  }
  s->nvars++;
  s->names = realloc(s->names, s->nvars * sizeof(char *));
  s->vals = realloc(s->vals, s->nvars * sizeof(Value));
  s->names[s->nvars - 1] = strdup(name);
  s->vals[s->nvars - 1] = val;
}
/* Returns 1 and sets *out (ref'd) if found; 0 otherwise. */
static int scope_get(Scope *s, const char *name, Value *out) {
  while (s) {
    for (int i = 0; i < s->nvars; i++) {
      if (strcmp(s->names[i], name) == 0) {
        *out = s->vals[i];
        v_ref(out);
        return 1;
      }
    }
    s = s->parent;
  }
  return 0;
}
/* Look up a name in the scope chain; returns an owned Value (ref'd), or sets
 * an "unbound symbol" error and returns nil. */
static Value scope_lookup(Scope *s, const char *name) {
  Value out;
  if (scope_get(s, name, &out))
    return out;
  set_err("unbound symbol '%s'", name);
  return v_nil();
}

/* ---- helpers ----------------------------------------------------------- */
static int v_truthy(Value *v) {
  return !(v->tag == V_NIL || (v->tag == V_BOOL && !v->u.b));
}
static const char *type_name(Value *v) {
  switch (v->tag) {
  case V_NIL:
    return "nil";
  case V_BOOL:
    return "bool";
  case V_INT:
    return "int";
  case V_FLOAT:
    return "float";
  case V_STR:
    return "str";
  case V_SYM:
    return "sym";
  case V_LIST:
    return "list";
  case V_MAP:
    return "hash";
  case V_BUILTIN:
    return "builtin";
  case V_CLOSURE:
    return "fn";
  }
  return "?";
}
static double as_f64(Value *v) {
  if (v->tag == V_INT)
    return (double)v->u.i;
  if (v->tag == V_FLOAT)
    return v->u.f;
  set_err("expected a number, got %s", type_name(v));
  return 0.0;
}

/* ---- checked integer arithmetic (i64 -> f64 promotion on overflow) ----- */

/* Number of UTF-8 characters (code points) in the first `len` bytes.
 * A byte with its high bit clear is a 1-byte character; a byte matching
 * 0b10xxxxxx continues the character started by the preceding lead byte and is
 * skipped. This is what makes `(len str)` count characters the way the
 * interpreter's `chars().count()` does instead of counting UTF-8 bytes. */
static size_t utf8_len(const char *s, size_t len) {
  size_t n = 0;
  for (size_t i = 0; i < len; i++) {
    if (((unsigned char)s[i] & 0xC0) != 0x80)
      n++;
  }
  return n;
}
static int checked_add(int64_t a, int64_t b, int64_t *out) {
  if (b > 0 && a > INT64_MAX - b)
    return 0;
  if (b < 0 && a < INT64_MIN - b)
    return 0;
  *out = a + b;
  return 1;
}
static int checked_sub(int64_t a, int64_t b, int64_t *out) {
  if (b > 0 && a < INT64_MIN + b)
    return 0;
  if (b < 0 && a > INT64_MAX + b)
    return 0;
  *out = a - b;
  return 1;
}
static int checked_mul(int64_t a, int64_t b, int64_t *out) {
  if (a == 0 || b == 0) {
    *out = 0;
    return 1;
  }
  /* Unsigned magnitude arithmetic, with the sign applied in unsigned space.
   *
   * Two traps, both reachable via INT64_MIN (whose magnitude is 2^63):
   *
   * 1. The magnitude product must not be computed before the overflow check.
   *    `(* 2 -9223372036854775808)` has magnitudes 2 and 2^63, whose product
   *    is 2^64 — which wraps uint64 to 0. Comparing that 0 against the limit
   *    passes, and the result became 0. Divide instead: ua*ub <= limit is
   *    exactly ub <= limit/ua for ua >= 1, so the product is only formed once
   *    it is known to fit (and since it is <= 2^63, it fits uint64 too).
   *
   * 2. The negation must not be `-(int64_t)ur`. When ur == 2^63 that value is
   *    not representable as int64_t, so the cast is implementation-defined and
   *    the negation is undefined — on clang it folded to 0. `(int64_t)(0 - ur)`
   *    computes the same bit pattern without UB, and is exactly INT64_MIN when
   *    ur == 2^63, which is the correct answer. */
  int neg = (a < 0) ^ (b < 0);
  uint64_t ua = a < 0 ? (uint64_t)(-(a + 1)) + 1 : (uint64_t)a;
  uint64_t ub = b < 0 ? (uint64_t)(-(b + 1)) + 1 : (uint64_t)b;
  uint64_t limit = neg ? (uint64_t)INT64_MAX + 1 : (uint64_t)INT64_MAX;
  if (ub > limit / ua)
    return 0; /* true product exceeds the i64 range -> caller promotes to f64 */
  uint64_t ur = ua * ub;
  *out = neg ? (int64_t)(0 - ur) : (int64_t)ur;
  return 1;
}

/* ---- equality (matches the interpreter's PartialEq) -------------------- */
static int values_eq(Value *a, Value *b) {
  if (a->tag != b->tag) {
    /* int/float cross-compare */
    if ((a->tag == V_INT && b->tag == V_FLOAT) ||
        (a->tag == V_FLOAT && b->tag == V_INT)) {
      double x = as_f64(a), y = as_f64(b);
      return x == y;
    }
    return 0;
  }
  switch (a->tag) {
  case V_NIL:
    return 1;
  case V_BOOL:
    return a->u.b == b->u.b;
  case V_INT:
    return a->u.i == b->u.i;
  case V_FLOAT:
    return a->u.f == b->u.f;
  case V_STR:
  case V_SYM:
    return a->u.s->len == b->u.s->len &&
           memcmp(a->u.s->data, b->u.s->data, a->u.s->len) == 0;
  case V_LIST: {
    ConsCell *ca = a->u.l, *cb = b->u.l;
    while (ca && cb && ca->len > 0 && cb->len > 0) {
      if (!values_eq(&ca->head, &cb->head))
        return 0;
      ca = ca->tail;
      cb = cb->tail;
    }
    return (ca == NULL || ca->len == 0) && (cb == NULL || cb->len == 0);
  }
  case V_MAP: {
    Map *ma = a->u.m, *mb = b->u.m;
    if (ma->n != mb->n)
      return 0;
    for (int i = 0; i < ma->n; i++) {
      if (!values_eq(&ma->keys[i], &mb->keys[i]) ||
          !values_eq(&ma->vals[i], &mb->vals[i]))
        return 0;
    }
    return 1;
  }
  default:
    return 0;
  }
}

/* ---- display (matches the interpreter's Display) ----------------------- */
/* Forward decl: value_repr (string-quoted variant) used by list/map render. */
static void value_repr(Value *v, char *buf, size_t bufsz);
/* Format a float like the interpreter: integer-valued finite -> "%.1f",
 * NaN/inf specially, otherwise the shortest round-trip decimal. */
static void format_float(double x, char *buf, size_t bufsz) {
  if (x != x) {
    snprintf(buf, bufsz, "NaN");
    return;
  }
  if (x == INFINITY) {
    snprintf(buf, bufsz, "inf");
    return;
  }
  if (x == -INFINITY) {
    snprintf(buf, bufsz, "-inf");
    return;
  }
  if (isfinite(x) && fmod(x, 1.0) == 0.0) {
    snprintf(buf, bufsz, "%.1f", x);
    return;
  }
  int neg = signbit(x);
  double ax = neg ? -x : x;
  if (ax == 0.0) {
    snprintf(buf, bufsz, neg ? "-0.0" : "0.0");
    return;
  }
  /* Shortest round-trip digits via %.*e (p digits after the point = p+1
   * significant figures). Rust's f64 Display uses the same shortest
   * round-trip digits but ALWAYS in fixed-point notation (never
   * scientific), so we reformat below. */
  char tmp[64];
  int best_p = 16;
  for (int p = 0; p <= 16; p++) {
    snprintf(tmp, sizeof(tmp), "%.*e", p, ax);
    if (strtod(tmp, NULL) == ax) {
      best_p = p;
      break;
    }
  }
  snprintf(tmp, sizeof(tmp), "%.*e", best_p, ax);
  /* Parse d.ddd e±exp. */
  char *p = tmp;
  char *e = strchr(p, 'e');
  int64_t exp10 = (int64_t)strtol(e + 1, NULL, 10);
  /* Mantissa digits (strip the dot). */
  char digits[32];
  int nd = 0;
  for (char *q = p; q < e; q++) {
    if (*q != '.')
      digits[nd++] = *q;
  }
  digits[nd] = 0;
  /* value = 0.digits * 10^(exp10+1); digits before the decimal point = exp10+1. */
  int64_t point = exp10 + 1;
  char out[4096];
  int oi = 0;
  if (point <= 0) {
    out[oi++] = '0';
    out[oi++] = '.';
    for (int k = 0; k < (int)(-point); k++)
      out[oi++] = '0';
    for (int k = 0; k < nd; k++)
      out[oi++] = digits[k];
  } else if (point >= nd) {
    for (int k = 0; k < nd; k++)
      out[oi++] = digits[k];
    for (int k = 0; k < (int)(point - nd); k++)
      out[oi++] = '0';
    out[oi++] = '.';
    out[oi++] = '0'; /* integer-valued -> ".0" */
  } else {
    for (int k = 0; k < (int)point; k++)
      out[oi++] = digits[k];
    out[oi++] = '.';
    for (int k = (int)point; k < nd; k++)
      out[oi++] = digits[k];
  }
  out[oi] = 0;
  if (neg) {
    memmove(out + 1, out, (size_t)oi);
    out[0] = '-';
    out[oi + 1] = 0;
  }
  snprintf(buf, bufsz, "%s", out);
}

/* Render a value to a string buffer (Display). Returns the buffer. */
static void value_to_string(Value *v, char *buf, size_t bufsz) {
  switch (v->tag) {
  case V_NIL:
    snprintf(buf, bufsz, "nil");
    break;
  case V_BOOL:
    snprintf(buf, bufsz, "%s", v->u.b ? "true" : "false");
    break;
  case V_INT:
    snprintf(buf, bufsz, "%lld", (long long)v->u.i);
    break;
  case V_FLOAT:
    format_float(v->u.f, buf, bufsz);
    break;
  case V_STR:
    snprintf(buf, bufsz, "%s", v->u.s->data);
    break;
  case V_SYM:
    snprintf(buf, bufsz, "%s", v->u.s->data);
    break;
  case V_LIST: {
    /* (elem elem ...) with each elem rendered via repr (strings quoted) */
    size_t off = 0;
    buf[off++] = '(';
    ConsCell *c = v->u.l;
    int first = 1;
    while (c && c->len > 0) {
      if (!first)
        buf[off++] = ' ';
      first = 0;
      char ebuf[8192];
      value_repr(&c->head, ebuf, sizeof(ebuf));
      off += (size_t)snprintf(buf + off, bufsz - off, "%s", ebuf);
      c = c->tail;
    }
    buf[off++] = ')';
    buf[off] = 0;
    break;
  }
  case V_MAP: {
    size_t off = 0;
    buf[off++] = '{';
    Map *m = v->u.m;
    for (int i = 0; i < m->n; i++) {
      if (i > 0)
        buf[off++] = ' ';
      char kb[8192], vb[8192];
      value_repr(&m->keys[i], kb, sizeof(kb));
      value_repr(&m->vals[i], vb, sizeof(vb));
      off += (size_t)snprintf(buf + off, bufsz - off, "%s %s", kb, vb);
    }
    buf[off++] = '}';
    buf[off] = 0;
    break;
  }
  case V_BUILTIN:
    snprintf(buf, bufsz, "<builtin>");
    break;
  case V_CLOSURE:
    snprintf(buf, bufsz, "<fn>");
    break;
  }
}
/* Like value_to_string but strings are quoted (used inside list/map render). */
static void value_repr(Value *v, char *buf, size_t bufsz) {
  if (v->tag == V_STR) {
    /* Rust {:?} for str: double-quoted with escapes */
    size_t off = 0;
    buf[off++] = '"';
    for (size_t k = 0; k < v->u.s->len; k++) {
      char ch = v->u.s->data[k];
      switch (ch) {
      case '"':
        buf[off++] = '\\';
        buf[off++] = '"';
        break;
      case '\\':
        buf[off++] = '\\';
        buf[off++] = '\\';
        break;
      case '\n':
        buf[off++] = '\\';
        buf[off++] = 'n';
        break;
      case '\t':
        buf[off++] = '\\';
        buf[off++] = 't';
        break;
      case '\r':
        buf[off++] = '\\';
        buf[off++] = 'r';
        break;
      default:
        buf[off++] = ch;
      }
    }
    buf[off++] = '"';
    buf[off] = 0;
  } else {
    value_to_string(v, buf, bufsz);
  }
}

/* ---- builtins ---------------------------------------------------------- */
static Value numeric_fold(Value *args, int nargs, int is_mul) {
  int64_t acc_i = is_mul ? 1 : 0;
  double acc_f = 0.0;
  int is_float = 0;
  for (int k = 0; k < nargs; k++) {
    Value *v = &args[k];
    if (v->tag == V_INT && !is_float) {
      int64_t i = v->u.i, r;
      int ok = is_mul ? checked_mul(acc_i, i, &r) : checked_add(acc_i, i, &r);
      if (ok) {
        acc_i = r;
      } else {
        is_float = 1;
        acc_f = is_mul ? (double)acc_i * (double)i : (double)acc_i + (double)i;
      }
    } else {
      double x = as_f64(v);
      if (g_err)
        return v_nil();
      if (!is_float) {
        is_float = 1;
        acc_f = (double)acc_i;
      }
      acc_f = is_mul ? acc_f * x : acc_f + x;
    }
  }
  return is_float ? v_float(acc_f) : v_int(acc_i);
}

static Value float_sub(Value *args, int nargs) {
  double acc = as_f64(&args[0]);
  if (g_err)
    return v_nil();
  for (int k = 1; k < nargs; k++) {
    double d = as_f64(&args[k]);
    if (g_err)
      return v_nil();
    acc -= d;
  }
  return v_float(acc);
}

static Value builtin_sub(Value *args, int nargs) {
  if (nargs == 0) {
    set_err("- expects at least 1 argument");
    return v_nil();
  }
  if (nargs == 1) {
    Value *a = &args[0];
    if (a->tag == V_INT) {
      if (a->u.i == INT64_MIN)
        return v_float(-(double)a->u.i);
      return v_int(-a->u.i);
    }
    if (a->tag == V_FLOAT)
      return v_float(-a->u.f);
    set_err("- expected number, got %s", type_name(a));
    return v_nil();
  }
  int all_int = (args[0].tag == V_INT);
  for (int k = 1; k < nargs && all_int; k++)
    all_int = (args[k].tag == V_INT);
  if (all_int) {
    int64_t acc = args[0].u.i, r;
    for (int k = 1; k < nargs; k++) {
      if (!checked_sub(acc, args[k].u.i, &r))
        return float_sub(args, nargs);
      acc = r;
    }
    return v_int(acc);
  }
  return float_sub(args, nargs);
}

static Value builtin_div(Value *args, int nargs) {
  if (nargs == 0) {
    set_err("/ expects at least 1 argument");
    return v_nil();
  }
  double acc = as_f64(&args[0]);
  if (g_err)
    return v_nil();
  if (nargs == 1)
    return v_float(1.0 / acc);
  for (int k = 1; k < nargs; k++) {
    double d = as_f64(&args[k]);
    if (g_err)
      return v_nil();
    if (d == 0.0) {
      set_err("division by zero");
      return v_nil();
    }
    acc /= d;
  }
  return v_float(acc);
}

static Value builtin_mod(Value *args, int nargs) {
  if (nargs != 2 || args[0].tag != V_INT || args[1].tag != V_INT) {
    set_err("mod expects (mod int int)");
    return v_nil();
  }
  int64_t a = args[0].u.i, b = args[1].u.i;
  if (b == 0) {
    set_err("mod by zero");
    return v_nil();
  }
  if (b == -1)
    return v_int(0);
  int64_t r = a % b;
  if (r < 0)
    r += (b < 0) ? -b : b; /* Euclidean: result in [0, |b|) */
  return v_int(r);
}

static Value builtin_eq(Value *args, int nargs) {
  for (int k = 0; k + 1 < nargs; k++) {
    if (!values_eq(&args[k], &args[k + 1]))
      return v_bool(0);
  }
  return v_bool(1);
}

static Value builtin_cmp(Value *args, int nargs, int op) {
  for (int k = 0; k + 1 < nargs; k++) {
    double a = as_f64(&args[k]);
    if (g_err)
      return v_nil();
    double b = as_f64(&args[k + 1]);
    if (g_err)
      return v_nil();
    if (isnan(a) || isnan(b)) {
      set_err("cannot compare NaN");
      return v_nil();
    }
    int keep;
    switch (op) {
    case 0:
      keep = a < b;
      break;
    case 1:
      keep = a > b;
      break;
    case 2:
      keep = a <= b;
      break;
    default:
      keep = a >= b;
      break;
    }
    if (!keep)
      return v_bool(0);
  }
  return v_bool(1);
}

static Value builtin_not(Value *args, int nargs) {
  if (nargs != 1) {
    set_err("expected 1 argument");
    return v_nil();
  }
  return v_bool(!v_truthy(&args[0]));
}

static Value builtin_print(Value *args, int nargs) {
  for (int k = 0; k < nargs; k++) {
    if (k > 0)
      fputc(' ', stdout);
    char buf[8192];
    value_to_string(&args[k], buf, sizeof(buf));
    fputs(buf, stdout);
  }
  fputc('\n', stdout);
  fflush(stdout);
  return v_nil();
}

static Value builtin_str(Value *args, int nargs) {
  size_t total = 0;
  for (int k = 0; k < nargs; k++) {
    char buf[8192];
    value_to_string(&args[k], buf, sizeof(buf));
    total += strlen(buf);
  }
  char *s = malloc(total + 1);
  s[0] = 0;
  for (int k = 0; k < nargs; k++) {
    char buf[8192];
    value_to_string(&args[k], buf, sizeof(buf));
    strcat(s, buf);
  }
  return v_str_take(s);
}

static Value builtin_list(Value *args, int nargs) {
  Value *items = malloc((nargs > 0 ? nargs : 1) * sizeof(Value));
  for (int k = 0; k < nargs; k++) {
    items[k] = args[k];
    v_ref(&items[k]);
  }
  Value r = v_list_from_array(items, nargs);
  free(items);
  return r;
}

static Value builtin_len(Value *args, int nargs) {
  if (nargs != 1) {
    set_err("expected 1 argument");
    return v_nil();
  }
  Value *a = &args[0];
  if (a->tag == V_LIST)
    return v_int((int64_t)a->u.l->len);
  if (a->tag == V_STR)
    /* UTF-8 *characters*, not bytes — `s->len` is the byte count, so "héllo"
     * would be 6 here against the interpreter's 5. Counts code points by
     * skipping continuation bytes (0b10xxxxxx), which is exact for valid UTF-8
     * and matches Rust's `chars().count()`. An invalid byte counts as one
     * character, which is also what Rust's lossy decode does. */
    return v_int((int64_t)utf8_len(a->u.s->data, a->u.s->len));
  if (a->tag == V_MAP)
    return v_int((int64_t)a->u.m->n);
  set_err("len expects list, str, or hash, got %s", type_name(a));
  return v_nil();
}

static Value builtin_first(Value *args, int nargs) {
  if (nargs != 1) {
    set_err("expected 1 argument");
    return v_nil();
  }
  Value *a = &args[0];
  if (a->tag != V_LIST) {
    set_err("first expects list, got %s", type_name(a));
    return v_nil();
  }
  if (a->u.l->len == 0)
    return v_nil();
  Value r = a->u.l->head;
  v_ref(&r);
  return r;
}

static Value builtin_rest(Value *args, int nargs) {
  if (nargs != 1) {
    set_err("expected 1 argument");
    return v_nil();
  }
  Value *a = &args[0];
  if (a->tag != V_LIST) {
    set_err("rest expects list, got %s", type_name(a));
    return v_nil();
  }
  if (a->u.l->tail) {
    Value r;
    r.tag = V_LIST;
    r.u.l = a->u.l->tail;
    v_ref(&r);
    return r;
  }
  return v_list_empty();
}

static Value builtin_nth(Value *args, int nargs) {
  if (nargs != 2 || args[0].tag != V_LIST || args[1].tag != V_INT) {
    set_err("nth expects (nth list int)");
    return v_nil();
  }
  int64_t i = args[1].u.i;
  if (i < 0)
    return v_nil();
  ConsCell *c = args[0].u.l;
  for (int64_t k = 0; k < i; k++) {
    if (!c->tail)
      return v_nil();
    c = c->tail;
  }
  if (c->len == 0)
    return v_nil();
  Value r = c->head;
  v_ref(&r);
  return r;
}

static Value builtin_cons(Value *args, int nargs) {
  if (nargs != 2 || args[1].tag != V_LIST) {
    set_err("cons expects (cons value list)");
    return v_nil();
  }
  Value head = args[0];
  v_ref(&head);
  Value r;
  r.tag = V_LIST;
  r.u.l = cons_cell_new(head, args[1].u.l);
  return r;
}

static Value builtin_push(Value *args, int nargs) {
  if (nargs < 1 || args[0].tag != V_LIST) {
    set_err("push expects (push list value...)");
    return v_nil();
  }
  int total = args[0].u.l->len + (nargs - 1);
  Value *items = malloc((total > 0 ? total : 1) * sizeof(Value));
  int idx = 0;
  ConsCell *c = args[0].u.l;
  while (c && c->len > 0) {
    items[idx] = c->head;
    v_ref(&items[idx]);
    idx++;
    c = c->tail;
  }
  for (int k = 1; k < nargs; k++) {
    items[idx] = args[k];
    v_ref(&items[idx]);
    idx++;
  }
  Value r = v_list_from_array(items, total);
  free(items);
  return r;
}

static Value as_map(Value *v, const char *who) {
  if (v->tag != V_MAP) {
    set_err("%s expects a hash, got %s", who, type_name(v));
    return v_nil();
  }
  return *v;
}

static Value builtin_hash(Value *args, int nargs) {
  if (nargs % 2 != 0) {
    set_err("hash expects an even number of key/value arguments, got %d",
            nargs);
    return v_nil();
  }
  int n = nargs / 2;
  Map *m = malloc(sizeof(Map));
  m->ref = 1;
  m->n = 0;
  m->keys = malloc((n > 0 ? n : 1) * sizeof(Value));
  m->vals = malloc((n > 0 ? n : 1) * sizeof(Value));
  for (int k = 0; k < nargs; k += 2) {
    Value key = args[k], val = args[k + 1];
    v_ref(&key);
    v_ref(&val);
    int found = -1;
    for (int i = 0; i < m->n; i++) {
      if (values_eq(&m->keys[i], &key)) {
        found = i;
        break;
      }
    }
    if (found >= 0) {
      v_unref(&m->vals[found]);
      m->vals[found] = val;
    } else {
      m->keys[m->n] = key;
      m->vals[m->n] = val;
      m->n++;
    }
  }
  Value r;
  r.tag = V_MAP;
  r.u.m = m;
  return r;
}

static Value builtin_get(Value *args, int nargs) {
  if (nargs != 2) {
    set_err("get expects (get hash key)");
    return v_nil();
  }
  Value h = as_map(&args[0], "get");
  if (g_err)
    return v_nil();
  Map *m = h.u.m;
  for (int i = 0; i < m->n; i++) {
    if (values_eq(&m->keys[i], &args[1])) {
      Value r = m->vals[i];
      v_ref(&r);
      return r;
    }
  }
  return v_nil();
}

static Value builtin_assoc(Value *args, int nargs) {
  if (nargs != 3) {
    set_err("assoc expects (assoc hash key value)");
    return v_nil();
  }
  Value h = as_map(&args[0], "assoc");
  if (g_err)
    return v_nil();
  Map *src = h.u.m;
  Map *m = malloc(sizeof(Map));
  m->ref = 1;
  m->n = src->n;
  m->keys = malloc((m->n + 1) * sizeof(Value));
  m->vals = malloc((m->n + 1) * sizeof(Value));
  for (int i = 0; i < src->n; i++) {
    m->keys[i] = src->keys[i];
    v_ref(&m->keys[i]);
    m->vals[i] = src->vals[i];
    v_ref(&m->vals[i]);
  }
  int found = -1;
  for (int i = 0; i < m->n; i++) {
    if (values_eq(&m->keys[i], &args[1])) {
      found = i;
      break;
    }
  }
  if (found >= 0) {
    v_unref(&m->vals[found]);
    Value nv = args[2];
    v_ref(&nv);
    m->vals[found] = nv;
  } else {
    Value nk = args[1], nv = args[2];
    v_ref(&nk);
    v_ref(&nv);
    m->keys[m->n] = nk;
    m->vals[m->n] = nv;
    m->n++;
  }
  Value r;
  r.tag = V_MAP;
  r.u.m = m;
  return r;
}

static Value builtin_has(Value *args, int nargs) {
  if (nargs != 2) {
    set_err("has expects (has hash key)");
    return v_nil();
  }
  Value h = as_map(&args[0], "has");
  if (g_err)
    return v_nil();
  Map *m = h.u.m;
  for (int i = 0; i < m->n; i++) {
    if (values_eq(&m->keys[i], &args[1]))
      return v_bool(1);
  }
  return v_bool(0);
}

static Value builtin_keys(Value *args, int nargs) {
  if (nargs != 1) {
    set_err("expected 1 argument");
    return v_nil();
  }
  Value h = as_map(&args[0], "keys");
  if (g_err)
    return v_nil();
  Map *m = h.u.m;
  Value *items = malloc((m->n > 0 ? m->n : 1) * sizeof(Value));
  for (int i = 0; i < m->n; i++) {
    items[i] = m->keys[i];
    v_ref(&items[i]);
  }
  Value r = v_list_from_array(items, m->n);
  free(items);
  return r;
}

static Value builtin_vals(Value *args, int nargs) {
  if (nargs != 1) {
    set_err("expected 1 argument");
    return v_nil();
  }
  Value h = as_map(&args[0], "vals");
  if (g_err)
    return v_nil();
  Map *m = h.u.m;
  Value *items = malloc((m->n > 0 ? m->n : 1) * sizeof(Value));
  for (int i = 0; i < m->n; i++) {
    items[i] = m->vals[i];
    v_ref(&items[i]);
  }
  Value r = v_list_from_array(items, m->n);
  free(items);
  return r;
}

static Value builtin_error(Value *args, int nargs) {
  char msg[2048];
  msg[0] = 0;
  for (int k = 0; k < nargs; k++) {
    if (k > 0)
      strcat(msg, " ");
    char buf[8192];
    value_to_string(&args[k], buf, sizeof(buf));
    strcat(msg, buf);
  }
  set_err("%s", msg);
  return v_nil();
}

/* ---- Stage 3.1 stdlib ---------------------------------------------------- */
/* Each of these is a byte-for-byte twin of the corresponding builtin in
 * crates/ainl-core/src/eval.rs, including the exact error strings. The two
 * rules that make the four backends agree rather than merely resemble each
 * other are applied here too:
 *   * ASCII-only case folding and whitespace trimming. <ctype.h>'s tolower/
 *     toupper are locale-dependent (and undefined for negative char values),
 *     and the three transpiler hosts each strip a different Unicode whitespace
 *     set, so both sides spell the character class out.
 *   * Where the hosts disagree (empty split separator, empty replace target,
 *     negative sqrt/sleep) this runtime raises the same message the
 *     interpreter does, instead of picking its own answer. */

/* The characters `trim` strips: space, tab, LF, CR, FF, VT (see is_ascii_ws in
 * eval.rs). Locale-independent by construction. */
static int a_is_ascii_ws(unsigned char c) {
  return c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '\x0C' ||
         c == '\x0B';
}

/* A str argument, or the shared "who expects a str, got t" error. Returns NULL
 * after setting g_err. */
static const char *as_str_arg(Value *v, const char *who) {
  if (v->tag == V_STR)
    return v->u.s->data;
  set_err("%s expects a str, got %s", who, type_name(v));
  return NULL;
}

/* A str *path* argument — the file builtins' wording. */
static const char *as_path_arg(Value *v, const char *who) {
  if (v->tag == V_STR)
    return v->u.s->data;
  set_err("%s expects a str path, got %s", who, type_name(v));
  return NULL;
}

/* The content operand of the two writing file builtins. */
static const char *as_content_arg(Value *v, const char *who) {
  if (v->tag == V_STR)
    return v->u.s->data;
  set_err("%s expects str content, got %s", who, type_name(v));
  return NULL;
}

/* A numeric argument reported under the builtin's own name. Returns 0 on
 * failure (g_err set); the caller must check. */
static int as_num_arg(Value *v, const char *who, double *out) {
  if (v->tag == V_INT) {
    *out = (double)v->u.i;
    return 1;
  }
  if (v->tag == V_FLOAT) {
    *out = v->u.f;
    return 1;
  }
  set_err("%s expects a number, got %s", who, type_name(v));
  return 0;
}

/* Is `buf[0..len)` well-formed UTF-8?
 *
 * The interpreter's read-file goes through std::fs::read_to_string, which
 * *fails* on invalid UTF-8 (the file's bytes would not be representable as a
 * Rust String). This runtime used to hand the raw bytes back instead, so
 * (read-file "invalid.bin") was a runtime error in the interpreter and 6 raw
 * bytes in the compiled binary — a real 4-backend divergence, reachable with
 * any file that is not valid UTF-8. Rejecting it here is what makes the two
 * agree. (The transpiler targets' hosts each have their own answer here, so
 * their helpers are pinned to this same rule — see docs/SYNTAX.md "File I/O".)
 *
 * UTF-8 validity: a lead byte is 0xxxxxxx, 110xxxxx, 1110xxxx or 11110xxx
 * followed by that many 10xxxxxx continuation bytes. Overlong encodings, a
 * surrogate half (ED A0..BF ..), and anything above U+10FFFF are rejected, per
 * the Unicode definition of well-formed UTF-8. */
static int utf8_valid(const char *buf, size_t len) {
  size_t i = 0;
  while (i < len) {
    unsigned char c = (unsigned char)buf[i];
    size_t need;
    unsigned int cp;
    if (c < 0x80) {
      i++;
      continue;
    } else if ((c & 0xE0) == 0xC0) {
      need = 1;
      cp = c & 0x1Fu;
    } else if ((c & 0xF0) == 0xE0) {
      need = 2;
      cp = c & 0x0Fu;
    } else if ((c & 0xF8) == 0xF0) {
      need = 3;
      cp = c & 0x07u;
    } else {
      return 0; /* continuation byte in lead position, or 0xF8-0xFF */
    }
    if (need >= len - i)
      return 0; /* truncated sequence: fewer bytes left than it needs */
    for (size_t k = 1; k <= need; k++) {
      unsigned char cc = (unsigned char)buf[i + k];
      if ((cc & 0xC0) != 0x80)
        return 0;
      cp = (cp << 6) | (cc & 0x3Fu);
    }
    /* Overlong (the shortest form must be used), surrogate halves, > U+10FFFF. */
    if (need == 1 && cp < 0x80)
      return 0;
    if (need == 2 && cp < 0x800)
      return 0;
    if (need == 3 && cp < 0x10000)
      return 0;
    if (cp >= 0xD800 && cp <= 0xDFFF)
      return 0;
    if (cp > 0x10FFFF)
      return 0;
    i += need + 1;
  }
  return 1;
}

/* Read a whole file as a new Str. fopen in "rb" so no CRLF translation can
 * change the bytes on a Windows host (matching the interpreter's read). */
static Value builtin_read_file(Value *args, int nargs) {
  if (nargs != 1) {
    set_err("read-file expects (read-file path)");
    return v_nil();
  }
  const char *path = as_path_arg(&args[0], "read-file");
  if (!path)
    return v_nil();
  FILE *f = fopen(path, "rb");
  if (!f) {
    set_err("read-file: cannot read '%s'", path);
    return v_nil();
  }
  size_t cap = 4096, len = 0;
  char *buf = malloc(cap);
  for (;;) {
    if (len + 4096 > cap) {
      cap *= 2;
      buf = realloc(buf, cap);
    }
    size_t got = fread(buf + len, 1, 4096, f);
    len += got;
    if (got < 4096)
      break;
  }
  int bad = ferror(f);
  fclose(f);
  if (bad) {
    free(buf);
    set_err("read-file: cannot read '%s'", path);
    return v_nil();
  }
  /* Same rule as the interpreter: a file whose bytes are not valid UTF-8
   * cannot become a string, so it is the same error as an unreadable path. */
  if (!utf8_valid(buf, len)) {
    free(buf);
    set_err("read-file: cannot read '%s'", path);
    return v_nil();
  }
  buf = realloc(buf, len + 1);
  buf[len] = 0;
  return v_str_take(buf);
}

/* write-file / append-file share one body: the mode string and the verb are the
 * only differences. */
static Value write_file_inner(Value *args, int nargs, const char *who,
                              const char *verb, const char *mode) {
  if (nargs != 2) {
    set_err("%s expects (%s path content)", who, who);
    return v_nil();
  }
  const char *path = as_path_arg(&args[0], who);
  if (!path)
    return v_nil();
  const char *content = as_content_arg(&args[1], who);
  if (!content)
    return v_nil();
  FILE *f = fopen(path, mode);
  if (!f) {
    set_err("%s: cannot %s '%s'", who, verb, path);
    return v_nil();
  }
  size_t n = strlen(content);
  size_t wrote = n ? fwrite(content, 1, n, f) : 0;
  int bad = (wrote != n) || fflush(f) != 0;
  if (fclose(f) != 0)
    bad = 1;
  if (bad) {
    set_err("%s: cannot %s '%s'", who, verb, path);
    return v_nil();
  }
  return v_nil();
}

static Value builtin_write_file(Value *args, int nargs) {
  /* "wb" truncates an existing file, matching write-file. */
  return write_file_inner(args, nargs, "write-file", "write", "wb");
}

static Value builtin_append_file(Value *args, int nargs) {
  /* "ab" appends, and creates the file when absent, matching append-file. */
  return write_file_inner(args, nargs, "append-file", "append to", "ab");
}

/* ---- Tier 1 file I/O -----------------------------------------------------
 * Byte-for-byte twins of the corresponding builtins in ainl-core/src/eval.rs,
 * including every error string. The path algebra (path_join/path_base/
 * path_dir) is spelled out here rather than delegated to <libgen.h>: the hosts
 * disagree on every edge case (see the measured table in eval.rs), and this
 * string surgery is the only way to get one answer all five backends share. */

#include <dirent.h>
#include <sys/stat.h>
#include <unistd.h>

/* path_canonical: the shared core of the three path builtins — duplicate
 * separators merged, "." segments dropped, a leading "/" kept, a trailing
 * separator dropped, and a trailing "." kept (so path_dir("a/.") is "a", not
 * the parent). Mirrors `path_canonical` in eval.rs exactly.
 *
 * Returns a malloc'd string, or NULL if allocation fails (callers treat that
 * as an allocation failure, which the runtime cannot report any better). */
static char *path_canonical(const char *p) {
  size_t len = strlen(p);
  int absolute = p[0] == '/';
  /* Upper bound: the input length is never exceeded by the canonical form
   * (a trailing "." re-added at the end is the one case that grows it, and
   * only for a path that already contained a "/."). len + 3 covers it. */
  char *out = malloc(len + 3);
  if (!out)
    return NULL;
  size_t w = 0;
  if (absolute)
    out[w++] = '/';
  size_t i = 0;
  while (i < len) {
    size_t j = i;
    while (j < len && p[j] != '/')
      j++;
    size_t seglen = j - i;
    int is_dot = (seglen == 1 && p[i] == '.');
    /* Drop an empty segment (duplicate separator) and a "." segment. */
    if (seglen > 0 && !is_dot) {
      if (w > (size_t)absolute && out[w - 1] != '/')
        out[w++] = '/';
      memcpy(out + w, p + i, seglen);
      w += seglen;
    }
    i = (j < len) ? j + 1 : j;
  }
  /* A trailing "." is the final component, and is kept — see above. */
  if (len >= 2 && p[len - 2] == '/' && p[len - 1] == '.') {
    if (w > (size_t)absolute && out[w - 1] != '/')
      out[w++] = '/';
    out[w++] = '.';
  }
  out[w] = 0;
  return out;
}

/* (path-join a b ...) -> str. */
static Value builtin_path_join(Value *args, int nargs) {
  if (nargs < 1) {
    set_err("path-join expects at least 1 argument");
    return v_nil();
  }
  size_t total = 0;
  for (int i = 0; i < nargs; i++) {
    if (args[i].tag != V_STR) {
      /* Reported by position, as in the interpreter. */
      set_err("path-join expects str parts, got %s at position %d", type_name(&args[i]),
              i + 1);
      return v_nil();
    }
    total += args[i].u.s->len;
  }
  /* nargs-1 separators for the join, +1 for the terminator. */
  char *buf = malloc(total + (size_t)nargs + 1);
  if (!buf) {
    set_err("path-join: out of memory");
    return v_nil();
  }
  size_t w = 0;
  for (int i = 0; i < nargs; i++) {
    if (i > 0)
      buf[w++] = '/';
    memcpy(buf + w, args[i].u.s->data, args[i].u.s->len);
    w += args[i].u.s->len;
  }
  buf[w] = 0;
  char *canon = path_canonical(buf);
  free(buf);
  if (!canon) {
    set_err("path-join: out of memory");
    return v_nil();
  }
  return v_str_take(canon);
}

/* (path-base p) -> the final component, or "" when there is none. */
static Value builtin_path_base(Value *args, int nargs) {
  if (nargs != 1) {
    set_err("path-base expects (path-base path)");
    return v_nil();
  }
  const char *path = as_path_arg(&args[0], "path-base");
  if (!path)
    return v_nil();
  char *canon = path_canonical(path);
  if (!canon) {
    set_err("path-base: out of memory");
    return v_nil();
  }
  char *last = strrchr(canon, '/');
  const char *name = last ? last + 1 : canon;
  Value r = v_str(name);
  free(canon);
  return r;
}

/* (path-dir p) -> everything before the final component; "/" for a top-level
 * name, "." when there is no separator at all. */
static Value builtin_path_dir(Value *args, int nargs) {
  if (nargs != 1) {
    set_err("path-dir expects (path-dir path)");
    return v_nil();
  }
  const char *path = as_path_arg(&args[0], "path-dir");
  if (!path)
    return v_nil();
  char *canon = path_canonical(path);
  if (!canon) {
    set_err("path-dir: out of memory");
    return v_nil();
  }
  char *last = strrchr(canon, '/');
  Value r;
  if (!last) {
    r = v_str(".");
  } else if (last == canon) {
    r = v_str("/");
  } else {
    *last = 0;
    r = v_str(canon);
  }
  free(canon);
  return r;
}

/* (file-exists p) -> true, or nil. lstat, not stat: a broken symlink is still
 * a directory entry, and following the link would report it as absent. */
static Value builtin_file_exists(Value *args, int nargs) {
  if (nargs != 1) {
    set_err("file-exists expects (file-exists path)");
    return v_nil();
  }
  const char *path = as_path_arg(&args[0], "file-exists");
  if (!path)
    return v_nil();
  struct stat st;
  /* A lone "/" must not be trimmed to "" (it is the root, which exists); a
   * trailing separator on anything else names the same entry, as in the
   * interpreter. The trimmed copy is built with malloc rather than by writing
   * into the caller's string, which is const. */
  size_t n = strlen(path);
  if (n > 1 && path[n - 1] == '/') {
    char *trimmed = malloc(n);
    if (!trimmed) {
      set_err("file-exists: out of memory");
      return v_nil();
    }
    memcpy(trimmed, path, n - 1);
    trimmed[n - 1] = 0;
    int ok = lstat(trimmed, &st) == 0;
    free(trimmed);
    return ok ? v_bool(1) : v_nil();
  }
  return lstat(path, &st) == 0 ? v_bool(1) : v_nil();
}

/* (delete-file p) -> nil. A directory is refused, not silently ignored, and a
 * missing path is an error — both matching the interpreter. */
static Value builtin_delete_file(Value *args, int nargs) {
  if (nargs != 1) {
    set_err("delete-file expects (delete-file path)");
    return v_nil();
  }
  const char *path = as_path_arg(&args[0], "delete-file");
  if (!path)
    return v_nil();
  struct stat st;
  if (lstat(path, &st) != 0) {
    set_err("delete-file: cannot delete '%s'", path);
    return v_nil();
  }
  if (S_ISDIR(st.st_mode)) {
    set_err("delete-file: cannot delete '%s': it is a directory", path);
    return v_nil();
  }
  if (unlink(path) != 0) {
    set_err("delete-file: cannot delete '%s'", path);
    return v_nil();
  }
  return v_nil();
}

/* Byte-value name order for list-dir's qsort. strcoll is locale-dependent
 * (and would disagree with the interpreter's Rust sort on several hosts), so
 * the comparison is unsigned-byte order — which is exactly what Rust's Ord
 * for str, Python's bytes ordering and a JS Buffer.compare all do. */
static int name_cmp(const void *a, const void *b) {
  const unsigned char *x = *(const unsigned char *const *)a;
  const unsigned char *y = *(const unsigned char *const *)b;
  while (*x && *x == *y) {
    x++;
    y++;
  }
  /* Cast to int so a difference of 0x80+ does not overflow to a positive. */
  return (int)*x - (int)*y;
}

/* (list-dir p) -> list of str, sorted by byte value. "." and ".." are
 * artifacts of the directory, not entries, so they are skipped — every other
 * backend includes every name, so all five agree. */
static Value builtin_list_dir(Value *args, int nargs) {
  if (nargs != 1) {
    set_err("list-dir expects (list-dir path)");
    return v_nil();
  }
  const char *path = as_path_arg(&args[0], "list-dir");
  if (!path)
    return v_nil();
  DIR *d = opendir(path);
  if (!d) {
    set_err("list-dir: cannot read '%s'", path);
    return v_nil();
  }
  size_t cap = 16, n = 0;
  char **names = malloc(cap * sizeof(char *));
  if (!names) {
    closedir(d);
    set_err("list-dir: out of memory");
    return v_nil();
  }
  struct dirent *ent;
  while ((ent = readdir(d)) != NULL) {
    if (strcmp(ent->d_name, ".") == 0 || strcmp(ent->d_name, "..") == 0)
      continue;
    if (n == cap) {
      cap *= 2;
      char **bigger = realloc(names, cap * sizeof(char *));
      if (!bigger) {
        /* Free what we have rather than leaking; the error is still reported. */
        for (size_t k = 0; k < n; k++)
          free(names[k]);
        free(names);
        closedir(d);
        set_err("list-dir: out of memory");
        return v_nil();
      }
      names = bigger;
    }
    names[n] = strdup(ent->d_name);
    if (!names[n]) {
      for (size_t k = 0; k < n; k++)
        free(names[k]);
      free(names);
      closedir(d);
      set_err("list-dir: out of memory");
      return v_nil();
    }
    n++;
  }
  closedir(d);
  /* Determinism is the whole point of the builtin: readdir order is
   * filesystem-dependent and differs per host. */
  if (n > 1)
    qsort(names, n, sizeof(char *), name_cmp);
  Value *items = malloc((n > 0 ? n : 1) * sizeof(Value));
  if (!items) {
    for (size_t k = 0; k < n; k++)
      free(names[k]);
    free(names);
    set_err("list-dir: out of memory");
    return v_nil();
  }
  for (size_t k = 0; k < n; k++) {
    items[k].tag = V_STR;
    items[k].u.s = NULL;
    Str *st = malloc(sizeof(Str));
    st->ref = 1;
    st->len = strlen(names[k]);
    st->data = names[k]; /* takes ownership */
    items[k].u.s = st;
  }
  free(names);
  return v_list_from_array(items, (int)n);
}

/* (split str sep) -> list of str. An empty separator is rejected, as in the
 * interpreter. */
static Value builtin_split(Value *args, int nargs) {
  if (nargs != 2) {
    set_err("split expects (split str separator)");
    return v_nil();
  }
  const char *s = as_str_arg(&args[0], "split");
  if (!s)
    return v_nil();
  const char *sep = as_str_arg(&args[1], "split");
  if (!sep)
    return v_nil();
  size_t seplen = strlen(sep);
  if (seplen == 0) {
    set_err("split expects a non-empty separator");
    return v_nil();
  }
  size_t slen = strlen(s);
  /* Upper bound on fields: (len / seplen) + 1, plus the empty-string case. */
  size_t maxf = slen / seplen + 2;
  Value *items = malloc(maxf * sizeof(Value));
  size_t n = 0, start = 0;
  for (size_t i = 0; i + seplen <= slen;) {
    if (memcmp(s + i, sep, seplen) == 0) {
      char *field = malloc(i - start + 1);
      memcpy(field, s + start, i - start);
      field[i - start] = 0;
      items[n].tag = V_STR;
      items[n].u.s = NULL;
      /* Build a Str around the malloc'd field so v_list_from_array can take
       * ownership uniformly. */
      Str *st = malloc(sizeof(Str));
      st->ref = 1;
      st->len = i - start;
      st->data = field;
      items[n].u.s = st;
      n++;
      i += seplen;
      start = i;
    } else {
      i++;
    }
  }
  /* The trailing field (possibly empty). */
  char *tail = malloc(slen - start + 1);
  memcpy(tail, s + start, slen - start);
  tail[slen - start] = 0;
  Str *st = malloc(sizeof(Str));
  st->ref = 1;
  st->len = slen - start;
  st->data = tail;
  items[n].tag = V_STR;
  items[n].u.s = st;
  n++;
  Value r = v_list_from_array(items, (int)n);
  free(items);
  return r;
}

/* (join list sep) -> str. Every element must be a str (matching the
 * interpreter, which errors rather than coercing like JS). */
static Value builtin_join(Value *args, int nargs) {
  if (nargs != 2) {
    set_err("join expects (join list separator)");
    return v_nil();
  }
  if (args[0].tag != V_LIST) {
    set_err("join expects a list, got %s", type_name(&args[0]));
    return v_nil();
  }
  if (args[1].tag != V_STR) {
    set_err("join expects a str separator, got %s", type_name(&args[1]));
    return v_nil();
  }
  const char *sep = args[1].u.s->data;
  size_t seplen = args[1].u.s->len;
  /* First pass: total length, validating every element is a str. */
  size_t total = 0;
  int count = 0;
  ConsCell *c = args[0].u.l;
  while (c && c->len > 0) {
    if (c->head.tag != V_STR) {
      set_err("join expects a list of str");
      return v_nil();
    }
    total += c->head.u.s->len;
    count++;
    c = c->tail;
  }
  if (count > 1)
    total += seplen * (size_t)(count - 1);
  char *out = malloc(total + 1);
  size_t off = 0;
  int first = 1;
  c = args[0].u.l;
  while (c && c->len > 0) {
    if (!first) {
      memcpy(out + off, sep, seplen);
      off += seplen;
    }
    first = 0;
    memcpy(out + off, c->head.u.s->data, c->head.u.s->len);
    off += c->head.u.s->len;
    c = c->tail;
  }
  out[off] = 0;
  return v_str_take(out);
}

/* (trim str) — strip leading/trailing ASCII whitespace (a_is_ascii_ws). */
static Value builtin_trim(Value *args, int nargs) {
  if (nargs != 1) {
    set_err("trim expects (trim str)");
    return v_nil();
  }
  const char *s = as_str_arg(&args[0], "trim");
  if (!s)
    return v_nil();
  const char *start = s;
  while (*start && a_is_ascii_ws((unsigned char)*start))
    start++;
  const char *end = s + strlen(s);
  while (end > start && a_is_ascii_ws((unsigned char)end[-1]))
    end--;
  size_t len = (size_t)(end - start);
  char *out = malloc(len + 1);
  memcpy(out, start, len);
  out[len] = 0;
  return v_str_take(out);
}

/* (replace str old new) — every non-overlapping occurrence, left to right.
 * The replacement is not rescanned, matching Rust's str::replace. An empty
 * target is rejected, as in the interpreter. */
static Value builtin_replace(Value *args, int nargs) {
  if (nargs != 3) {
    set_err("replace expects (replace str old new)");
    return v_nil();
  }
  const char *s = as_str_arg(&args[0], "replace");
  if (!s)
    return v_nil();
  const char *old = as_str_arg(&args[1], "replace");
  if (!old)
    return v_nil();
  const char *neu = as_str_arg(&args[2], "replace");
  if (!neu)
    return v_nil();
  size_t oldlen = strlen(old), newlen = strlen(neu), slen = strlen(s);
  if (oldlen == 0) {
    set_err("replace expects a non-empty target");
    return v_nil();
  }
  /* Worst case: every byte is a match and each grows to newlen. */
  size_t cap = slen * (newlen + 1) + 1;
  char *out = malloc(cap);
  size_t off = 0;
  for (size_t i = 0; i < slen;) {
    if (i + oldlen <= slen && memcmp(s + i, old, oldlen) == 0) {
      memcpy(out + off, neu, newlen);
      off += newlen;
      i += oldlen;
    } else {
      out[off++] = s[i++];
    }
  }
  out[off] = 0;
  return v_str_take(out);
}

/* (upcase str) / (downcase str) — ASCII only (see a_is_ascii_ws). The explicit
 * ranges avoid <ctype.h>'s locale dependency and its negative-char UB. */
static Value builtin_case(Value *args, int nargs, int up) {
  const char *who = up ? "upcase" : "downcase";
  if (nargs != 1) {
    set_err("%s expects (%s str)", who, who);
    return v_nil();
  }
  const char *s = as_str_arg(&args[0], who);
  if (!s)
    return v_nil();
  size_t len = strlen(s);
  char *out = malloc(len + 1);
  for (size_t i = 0; i < len; i++) {
    unsigned char c = (unsigned char)s[i];
    if (up)
      out[i] = (c >= 'a' && c <= 'z') ? (char)(c - 32) : (char)c;
    else
      out[i] = (c >= 'A' && c <= 'Z') ? (char)(c + 32) : (char)c;
  }
  out[len] = 0;
  return v_str_take(out);
}

static Value builtin_upcase(Value *args, int nargs) {
  return builtin_case(args, nargs, 1);
}
static Value builtin_downcase(Value *args, int nargs) {
  return builtin_case(args, nargs, 0);
}

/* (contains hay needle) -> bool. An empty needle is true, matching strstr. */
static Value builtin_contains(Value *args, int nargs) {
  if (nargs != 2) {
    set_err("contains expects (contains str sub)");
    return v_nil();
  }
  const char *hay = as_str_arg(&args[0], "contains");
  if (!hay)
    return v_nil();
  const char *needle = as_str_arg(&args[1], "contains");
  if (!needle)
    return v_nil();
  return v_bool(strstr(hay, needle) != NULL);
}

/* (env-get name) -> str or nil. */
static Value builtin_env_get(Value *args, int nargs) {
  if (nargs != 1) {
    set_err("env-get expects (env-get name)");
    return v_nil();
  }
  const char *name = as_str_arg(&args[0], "env-get");
  if (!name)
    return v_nil();
  const char *v = getenv(name);
  return v ? v_str(v) : v_nil();
}

/* (exit code) — does not return. stdout is flushed first: exit() does not
 * flush stdio the way a normal return does on every platform. */
static Value builtin_exit(Value *args, int nargs) {
  if (nargs != 1) {
    set_err("exit expects (exit code)");
    return v_nil();
  }
  if (args[0].tag != V_INT) {
    set_err("exit expects an int, got %s", type_name(&args[0]));
    return v_nil();
  }
  fflush(stdout);
  fflush(stderr);
  exit((int)args[0].u.i);
}

/* (now) -> whole seconds since the Unix epoch. */
static Value builtin_now(Value *args, int nargs) {
  if (nargs != 0) {
    set_err("now expects (now)");
    return v_nil();
  }
  return v_int((int64_t)time(NULL));
}

/* (sleep seconds) -> nil. Negative and NaN are rejected so all four backends
 * agree. Long requests are split into <= 1-year naps: nanosleep/select take a
 * time_t, and an absurd value like 1e300 would otherwise wrap. */
static Value builtin_sleep(Value *args, int nargs) {
  if (nargs != 1) {
    set_err("sleep expects (sleep seconds)");
    return v_nil();
  }
  double secs;
  if (!as_num_arg(&args[0], "sleep", &secs))
    return v_nil();
  if (isnan(secs) || secs < 0.0) {
    set_err("sleep expects a non-negative number");
    return v_nil();
  }
  while (secs > 0.0) {
    double chunk = secs > 31536000.0 ? 31536000.0 : secs;
    struct timespec ts;
    ts.tv_sec = (time_t)chunk;
    ts.tv_nsec = (long)((chunk - (double)ts.tv_sec) * 1e9);
    if (ts.tv_nsec < 0)
      ts.tv_nsec = 0;
    if (ts.tv_nsec > 999999999L)
      ts.tv_nsec = 999999999L;
    if (nanosleep(&ts, NULL) != 0)
      break; /* interrupted by a signal: stop rather than spin */
    secs -= chunk;
  }
  return v_nil();
}

/* (abs n) — integer-preserving; abs(INT64_MIN) has no i64 answer, so it
 * promotes to float exactly as the interpreter's checked_abs does. */
static Value builtin_abs(Value *args, int nargs) {
  if (nargs != 1) {
    set_err("abs expects (abs n)");
    return v_nil();
  }
  Value *a = &args[0];
  if (a->tag == V_INT) {
    if (a->u.i == INT64_MIN)
      return v_float(-(double)a->u.i);
    return v_int(a->u.i < 0 ? -a->u.i : a->u.i);
  }
  if (a->tag == V_FLOAT)
    return v_float(fabs(a->u.f));
  set_err("abs expects a number, got %s", type_name(a));
  return v_nil();
}

/* (min a b ...) / (max a b ...) — numeric only, at least one argument, folding
 * pairwise. Ties keep the first of the equal values (strict < / >), matching
 * the interpreter. Returns an *owned* Value, so the winner is ref'd. */
static Value builtin_minmax(Value *args, int nargs, int max) {
  const char *who = max ? "max" : "min";
  if (nargs < 1) {
    set_err("%s expects at least 1 argument", who);
    return v_nil();
  }
  double best;
  if (!as_num_arg(&args[0], who, &best))
    return v_nil();
  int bi = 0;
  for (int k = 1; k < nargs; k++) {
    double x;
    if (!as_num_arg(&args[k], who, &x))
      return v_nil();
    if (max ? (x > best) : (x < best)) {
      best = x;
      bi = k;
    }
  }
  Value r = args[bi];
  v_ref(&r);
  return r;
}

static Value builtin_min(Value *args, int nargs) {
  return builtin_minmax(args, nargs, 0);
}
static Value builtin_max(Value *args, int nargs) {
  return builtin_minmax(args, nargs, 1);
}

/* (floor n) -> int. An int passes through unchanged (no double round-trip, so
 * a large i64 keeps every bit). */
static Value builtin_floor(Value *args, int nargs) {
  if (nargs != 1) {
    set_err("floor expects (floor n)");
    return v_nil();
  }
  if (args[0].tag == V_INT)
    return v_int(args[0].u.i);
  double x;
  if (!as_num_arg(&args[0], "floor", &x))
    return v_nil();
  double f = floor(x);
  if (f >= -9223372036854775808.0 && f < 9223372036854775808.0)
    return v_int((int64_t)f);
  return v_float(f);
}

/* (sqrt n) -> float. Negative is an error, not a silent NaN. */
static Value builtin_sqrt(Value *args, int nargs) {
  if (nargs != 1) {
    set_err("sqrt expects (sqrt n)");
    return v_nil();
  }
  double x;
  if (!as_num_arg(&args[0], "sqrt", &x))
    return v_nil();
  if (x < 0.0) {
    set_err("sqrt expects a non-negative number");
    return v_nil();
  }
  return v_float(sqrt(x));
}

/* ---- call dispatch ----------------------------------------------------- */
static Value v_call(Value callee, Value *args, int nargs) {
  tick(); /* bounds recursion / runaway calls */
  if (g_err)
    return v_nil();
  if (callee.tag == V_BUILTIN) {
    switch (callee.u.builtin) {
    case B_ADD:
      return numeric_fold(args, nargs, 0);
    case B_MUL:
      return numeric_fold(args, nargs, 1);
    case B_SUB:
      return builtin_sub(args, nargs);
    case B_DIV:
      return builtin_div(args, nargs);
    case B_EQ:
      return builtin_eq(args, nargs);
    case B_LT:
      return builtin_cmp(args, nargs, 0);
    case B_GT:
      return builtin_cmp(args, nargs, 1);
    case B_LE:
      return builtin_cmp(args, nargs, 2);
    case B_GE:
      return builtin_cmp(args, nargs, 3);
    case B_NOT:
      return builtin_not(args, nargs);
    case B_MOD:
      return builtin_mod(args, nargs);
    case B_PRINT:
      return builtin_print(args, nargs);
    case B_STR:
      return builtin_str(args, nargs);
    case B_LIST:
      return builtin_list(args, nargs);
    case B_LEN:
      return builtin_len(args, nargs);
    case B_FIRST:
      return builtin_first(args, nargs);
    case B_REST:
      return builtin_rest(args, nargs);
    case B_NTH:
      return builtin_nth(args, nargs);
    case B_CONS:
      return builtin_cons(args, nargs);
    case B_PUSH:
      return builtin_push(args, nargs);
    case B_HASH:
      return builtin_hash(args, nargs);
    case B_GET:
      return builtin_get(args, nargs);
    case B_ASSOC:
      return builtin_assoc(args, nargs);
    case B_HAS:
      return builtin_has(args, nargs);
    case B_KEYS:
      return builtin_keys(args, nargs);
    case B_VALS:
      return builtin_vals(args, nargs);
    case B_ERROR:
      return builtin_error(args, nargs);
    /* Stage 3.1 stdlib */
    case B_READ_FILE:
      return builtin_read_file(args, nargs);
    case B_WRITE_FILE:
      return builtin_write_file(args, nargs);
    case B_APPEND_FILE:
      return builtin_append_file(args, nargs);
    /* Tier 1 file I/O */
    case B_FILE_EXISTS:
      return builtin_file_exists(args, nargs);
    case B_DELETE_FILE:
      return builtin_delete_file(args, nargs);
    case B_LIST_DIR:
      return builtin_list_dir(args, nargs);
    case B_PATH_JOIN:
      return builtin_path_join(args, nargs);
    case B_PATH_BASE:
      return builtin_path_base(args, nargs);
    case B_PATH_DIR:
      return builtin_path_dir(args, nargs);
    case B_SPLIT:
      return builtin_split(args, nargs);
    case B_JOIN:
      return builtin_join(args, nargs);
    case B_TRIM:
      return builtin_trim(args, nargs);
    case B_REPLACE:
      return builtin_replace(args, nargs);
    case B_UPCASE:
      return builtin_upcase(args, nargs);
    case B_DOWNCASE:
      return builtin_downcase(args, nargs);
    case B_CONTAINS:
      return builtin_contains(args, nargs);
    case B_ENV_GET:
      return builtin_env_get(args, nargs);
    case B_EXIT:
      return builtin_exit(args, nargs);
    case B_NOW:
      return builtin_now(args, nargs);
    case B_SLEEP:
      return builtin_sleep(args, nargs);
    case B_ABS:
      return builtin_abs(args, nargs);
    case B_MIN:
      return builtin_min(args, nargs);
    case B_MAX:
      return builtin_max(args, nargs);
    case B_FLOOR:
      return builtin_floor(args, nargs);
    case B_SQRT:
      return builtin_sqrt(args, nargs);
    default:
      set_err("unknown builtin");
      return v_nil();
    }
  }
  if (callee.tag == V_CLOSURE) {
    Closure *c = callee.u.c;
    int np = c->nparams;
    if (c->variadic) {
      if (nargs < np) {
        set_err("fn expects at least %d args, got %d", np, nargs);
        return v_nil();
      }
    } else if (nargs != np) {
      set_err("fn expects %d args, got %d", np, nargs);
      return v_nil();
    }
    Scope *call_env = scope_new(c->def_env);
    for (int k = 0; k < np; k++) {
      Value a = args[k];
      v_ref(&a);
      scope_define(call_env, c->params[k], a);
    }
    if (c->variadic) {
      int extra = nargs - np;
      Value *rest = malloc((extra > 0 ? extra : 1) * sizeof(Value));
      for (int k = np; k < nargs; k++) {
        rest[k - np] = args[k];
        v_ref(&rest[k - np]);
      }
      Value rl = v_list_from_array(rest, extra);
      free(rest);
      Value rl2 = rl;
      v_ref(&rl2);
      scope_define(call_env, c->variadic, rl2);
    }
    Value result = c->fn(call_env, args, nargs);
    scope_unref(call_env);
    return result;
  }
  set_err("cannot call a %s", type_name(&callee));
  return v_nil();
}

/* ---- tight 2-arg arithmetic/comparison (hot-path inlines) -------------- */
/* These mirror the interpreter's 2-arg builtin semantics exactly (checked
 * i64 with promotion to f64 on overflow) but avoid the generic v_call switch
 * dispatch so the AOT hot loop stays tight.
 *
 * Every one of them type-checks its operands first. The arithmetic inlines
 * used to skip that and read `u.f` unconditionally on the non-int path, which
 * reinterprets a V_STR/V_LIST/... union member (a pointer) as a double: a
 * silent UB read that made `(+ 1 "a")` return 1.0 instead of raising
 * "expected a number, got str". `a_cmp_ok` has always checked; these now
 * share its type test. */

static inline int a_is_num(Value v) {
  return v.tag == V_INT || v.tag == V_FLOAT;
}

/* Set the type error the interpreter would raise, naming the first
 * non-numeric operand (left-to-right, as the interpreter does). */
static inline int a_num_fail(Value a, Value b) {
  if (!a_is_num(a)) {
    set_err("expected a number, got %s", type_name(&a));
  } else {
    set_err("expected a number, got %s", type_name(&b));
  }
  return 0;
}

static inline Value a_add(Value a, Value b) {
  if (a.tag == V_INT && b.tag == V_INT) {
    int64_t r;
    if (checked_add(a.u.i, b.u.i, &r))
      return v_int(r);
    return v_float((double)a.u.i + (double)b.u.i);
  }
  if (!a_is_num(a) || !a_is_num(b)) {
    a_num_fail(a, b);
    return v_nil();
  }
  double x = a.tag == V_INT ? (double)a.u.i : a.u.f;
  double y = b.tag == V_INT ? (double)b.u.i : b.u.f;
  return v_float(x + y);
}
static inline Value a_sub2(Value a, Value b) {
  if (a.tag == V_INT && b.tag == V_INT) {
    int64_t r;
    if (checked_sub(a.u.i, b.u.i, &r))
      return v_int(r);
    return v_float((double)a.u.i - (double)b.u.i);
  }
  if (!a_is_num(a) || !a_is_num(b)) {
    a_num_fail(a, b);
    return v_nil();
  }
  double x = a.tag == V_INT ? (double)a.u.i : a.u.f;
  double y = b.tag == V_INT ? (double)b.u.i : b.u.f;
  return v_float(x - y);
}
static inline Value a_mul(Value a, Value b) {
  if (a.tag == V_INT && b.tag == V_INT) {
    int64_t r;
    if (checked_mul(a.u.i, b.u.i, &r))
      return v_int(r);
    return v_float((double)a.u.i * (double)b.u.i);
  }
  if (!a_is_num(a) || !a_is_num(b)) {
    a_num_fail(a, b);
    return v_nil();
  }
  double x = a.tag == V_INT ? (double)a.u.i : a.u.f;
  double y = b.tag == V_INT ? (double)b.u.i : b.u.f;
  return v_float(x * y);
}
static inline Value a_div(Value a, Value b) {
  if (!a_is_num(a) || !a_is_num(b)) {
    a_num_fail(a, b);
    return v_nil();
  }
  double x = a.tag == V_INT ? (double)a.u.i : a.u.f;
  double y = b.tag == V_INT ? (double)b.u.i : b.u.f;
  if (y == 0.0) {
    set_err("division by zero");
    return v_nil();
  }
  return v_float(x / y);
}
static inline int a_cmp_ok(Value a, Value b, double *px, double *py) {
  if (!a_is_num(a) || !a_is_num(b)) {
    a_num_fail(a, b);
    return 0;
  }
  *px = a.tag == V_INT ? (double)a.u.i : a.u.f;
  *py = b.tag == V_INT ? (double)b.u.i : b.u.f;
  if (isnan(*px) || isnan(*py)) {
    set_err("cannot compare NaN");
    return 0;
  }
  return 1;
}
static inline Value a_lt(Value a, Value b) {
  double x, y;
  if (!a_cmp_ok(a, b, &x, &y))
    return v_nil();
  return v_bool(x < y);
}
static inline Value a_gt(Value a, Value b) {
  double x, y;
  if (!a_cmp_ok(a, b, &x, &y))
    return v_nil();
  return v_bool(x > y);
}
static inline Value a_le(Value a, Value b) {
  double x, y;
  if (!a_cmp_ok(a, b, &x, &y))
    return v_nil();
  return v_bool(x <= y);
}
static inline Value a_ge(Value a, Value b) {
  double x, y;
  if (!a_cmp_ok(a, b, &x, &y))
    return v_nil();
  return v_bool(x >= y);
}
static inline Value a_eq2(Value a, Value b) { return v_bool(values_eq(&a, &b)); }
static inline Value a_mod(Value a, Value b) {
  if (a.tag != V_INT || b.tag != V_INT) {
    set_err("mod expects (mod int int)");
    return v_nil();
  }
  int64_t x = a.u.i, y = b.u.i;
  if (y == 0) {
    set_err("mod by zero");
    return v_nil();
  }
  if (y == -1)
    return v_int(0);
  int64_t r = x % y;
  if (r < 0)
    r += (y < 0) ? -y : y; /* Euclidean: result in [0, |y|) */
  return v_int(r);
}

/* Copy a dense slot and take a ref (yields an owned Value). */
static inline Value slot_ref(Value *locals, int i) {
  Value v = locals[i];
  v_ref(&v);
  return v;
}
/* Store an owned Value into a dense slot, unref'ing the old value. */
static inline void slot_set(Value *locals, int i, Value val) {
  v_unref(&locals[i]);
  locals[i] = val;
}

/* ---- prelude ----------------------------------------------------------- */
static void scope_install_prelude(Scope *env) {
  struct {
    const char *name;
    int id;
  } table[] = {
      {"+", B_ADD},   {"*", B_MUL}, {"-", B_SUB}, {"/", B_DIV},
      {"=", B_EQ},    {"<", B_LT},  {">", B_GT},  {"<=", B_LE},
      {">=", B_GE},   {"not", B_NOT}, {"mod", B_MOD}, {"print", B_PRINT},
      {"str", B_STR}, {"list", B_LIST}, {"len", B_LEN}, {"first", B_FIRST},
      {"rest", B_REST}, {"nth", B_NTH}, {"cons", B_CONS}, {"push", B_PUSH},
      {"hash", B_HASH}, {"get", B_GET}, {"assoc", B_ASSOC}, {"has", B_HAS},
      {"keys", B_KEYS}, {"vals", B_VALS}, {"error", B_ERROR},
      /* Stage 3.1 stdlib */
      {"read-file", B_READ_FILE}, {"write-file", B_WRITE_FILE},
      {"append-file", B_APPEND_FILE}, {"split", B_SPLIT}, {"join", B_JOIN},
      {"trim", B_TRIM}, {"replace", B_REPLACE}, {"upcase", B_UPCASE},
      {"downcase", B_DOWNCASE}, {"contains", B_CONTAINS},
      {"env-get", B_ENV_GET}, {"exit", B_EXIT}, {"now", B_NOW},
      {"sleep", B_SLEEP}, {"abs", B_ABS}, {"min", B_MIN}, {"max", B_MAX},
      {"floor", B_FLOOR}, {"sqrt", B_SQRT},
      /* Tier 1 file I/O */
      {"file-exists", B_FILE_EXISTS}, {"delete-file", B_DELETE_FILE},
      {"list-dir", B_LIST_DIR}, {"path-join", B_PATH_JOIN},
      {"path-base", B_PATH_BASE}, {"path-dir", B_PATH_DIR},
  };
  for (size_t i = 0; i < sizeof(table) / sizeof(table[0]); i++) {
    Value b;
    b.tag = V_BUILTIN;
    b.u.builtin = table[i].id;
    Value bb = b;
    v_ref(&bb);
    scope_define(env, table[i].name, bb);
  }
}
