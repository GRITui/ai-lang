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
  B_HASH, B_GET, B_ASSOC, B_HAS, B_KEYS, B_VALS, B_ERROR, B_COUNT
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
    return v_int((int64_t)a->u.s->len);
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
