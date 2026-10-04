/* AINL AOT micro-runtime.
 *
 * Inlined into every generated .c file so the compiled binary is standalone
 * (zero runtime deps beyond libc). Faithfully ports the interpreter's Value
 * model (nil/bool/int/float/str/sym/list/map/builtin/closure), lexical scopes
 * with a parent chain (closures capture their defining scope), cons-cell
 * lists, and the numeric model (arbitrary-precision integers that widen past
 * i64, per docs/NUMERIC_MODEL.md).
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
  V_CLOSURE, V_BIGINT
} VTag;

typedef struct Value Value;
typedef struct Str Str;
typedef struct ConsCell ConsCell;
typedef struct Map Map;
typedef struct BigNum BigNum; /* arbitrary-precision int, refcounted */

/* Forward decls: the bignum module (defined below, after the checked-int
 * helpers) is needed by v_ref/v_unref and the numeric ops. */
static void bignum_unref(BigNum *bn);
static void bignum_ref(BigNum *bn);
static double bignum_to_f64(BigNum *bn);
static void bignum_to_string(BigNum *bn, char *buf, size_t bufsz);
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
    BigNum *bn; /* V_BIGINT: arbitrary-precision integer (refcounted) */
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
  /* Tier 1 JSON. Appended at the end for the same reason as the Tier 1 file
   * ids above: every earlier id keeps the value it has always had. */
  B_JSON_PARSE, B_JSON_SERIALIZE,
  /* Tier 2 testing. Appended last for the same reason. */
  B_TEST,
  /* Tier 3 collections. Appended last for the same reason. `sort` is the only
   * one of the four collection operations that is a real builtin; `map`,
   * `filter` and `reduce` are special forms lowered to loops before codegen
   * (see ainl_core::collection_forms), so they need no id here. */
  B_SORT,
  /* Byte-oriented string primitives (Tier 3). Appended last for the same
   * reason. */
  B_SUBSTRING, B_CHAR, B_CODE, B_STARTS_WITH, B_ENDS_WITH, B_INDEX_OF,
  /* Tier 3 file system. Appended last for the same reason: the five ids and
   * BUILTIN_IDS in ainl-cc's lib.rs are extended in exactly this order, and
   * crates/ainl-cc/tests/aot_stdlib.rs checks both directions. */
  B_MKDIR, B_RENAME, B_COPY, B_IS_DIR, B_FILE_SIZE,
  /* Tier 4 storage. Appended last for the same reason: the five ids and
   * BUILTIN_IDS in ainl-cc's lib.rs are extended in exactly this order. Unlike
   * `import` and the HTTP pair, these are NOT refused here — the engine is a
   * header, an append-only log and a CRC per record, which is all libc. The
   * implementation is the `db_*` block further down, a hand-port of
   * ainl-core/src/db.rs. */
  B_DB_OPEN, B_DB_PUT, B_DB_GET, B_DB_FLUSH, B_DB_CLOSE,
  /* Tier 4 key-value layer. Appended last for the same reason. `db-get` is NOT
   * among them: that id already exists above and the value layer takes over
   * that one name rather than getting a second id, which is exactly what makes
   * `db-set`/`db-get` a round-tripping pair in a compiled binary. */
  B_DB_SET, B_DB_GET_RAW, B_DB_DEL, B_DB_KEYS, B_DB_COUNT,
  /* Tier 4 table layer. Appended in the same order as BUILTIN_IDS in
   * ainl-cc/src/lib.rs, which is the only thing that has to agree: a builtin id
   * is a position in this enum, so adding one name in one place and not the
   * other would silently renumber every id after it and break every compiled
   * binary. The table layer rebinds no existing name, so unlike the value layer
   * there is no ordering constraint to reason about. */
  B_DB_CREATE_TABLE, B_DB_INSERT, B_DB_SELECT, B_DB_DELETE_ROW, B_DB_ALL_ROWS,
  /* The query layer. Also appended in BUILTIN_IDS order, and also rebinding no
   * existing name — `db-query` and `db-query-count` are new words, and the
   * refusal to call them a language construct is why the transpilers say no to
   * them while the interpreter and the AOT runtime both say yes. */
  B_DB_QUERY, B_DB_QUERY_COUNT,
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
  case V_BIGINT:
    bignum_ref(v->u.bn);
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
  case V_BIGINT:
    bignum_unref(v->u.bn);
    break;
  default:
    break;
  }
}

/* Free a Map at refcount zero. json-parse builds Maps by hand while filling
 * them, and a parse error part-way through has to release the partial map —
 * v_unref can't be used there because the Value is never returned. */
static void map_free(Map *m) {
  if (!m)
    return;
  for (int i = 0; i < m->n; i++) {
    v_unref(&m->keys[i]);
    v_unref(&m->vals[i]);
  }
  free(m->keys);
  free(m->vals);
  free(m);
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
/* Wrap a BigNum (ref >= 1) as a Value. Takes the reference. */
static Value v_bigint(BigNum *bn) {
  Value v;
  v.tag = V_BIGINT;
  v.u.bn = bn;
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
  case V_BIGINT:
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
  if (v->tag == V_BIGINT)
    return bignum_to_f64(v->u.bn);
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

/* ---- arbitrary-precision integers (matches ainl-core's BigNum) --------- */
/* A line-for-line port of crates/ainl-core/src/bignum.rs. The interpreter's
 * integers are arbitrary-precision: `Small(i64)` fast path + `Big` signed-
 * magnitude base-2^32 limbs, refcounted. The C runtime mirrors that exactly
 * so AOT results match the interpreter digit-for-digit (docs/NUMERIC_MODEL.md).
 *
 * Invariants (same as the Rust type):
 *   - a zero value is always the Small(0) fast path, never a Big with empty
 *     limbs;
 *   - Big limbs are normalized (no trailing zero limbs);
 *   - every BigNum is refcounted: bignum_new_* returns ref 1, bignum_ref
 *     bumps, bignum_unref drops (freeing at 0).
 *
 * The a_* hot-path ops and the builtins take ownership of their Value args
 * (the generated code unrefs them after the call), so the ops may consume the
 * operands' BigNums. */
struct BigNum {
  int ref;
  int is_small; /* 1 => i is valid; 0 => limbs (signed-magnitude) */
  int64_t i;    /* small fast path */
  int neg;      /* Big: sign */
  uint32_t *limbs;
  size_t nlimbs;
};

static BigNum *bignum_new_small(int64_t i) {
  BigNum *bn = malloc(sizeof(BigNum));
  bn->ref = 1;
  bn->is_small = 1;
  bn->i = i;
  bn->neg = 0;
  bn->limbs = NULL;
  bn->nlimbs = 0;
  return bn;
}
/* Wrap a computed BigNum as a Value, narrowing Small -> V_INT (indexable) and
 * keeping Big as V_BIGINT (not indexable), mirroring as_i64() semantics. */
static Value v_from_bignum(BigNum *bn) {
  if (bn->is_small) {
    Value v = v_int(bn->i);
    bignum_unref(bn);
    return v;
  }
  return v_bigint(bn);
}
static BigNum *bignum_new_big(int neg, const uint32_t *limbs, size_t nlimbs) {
  BigNum *bn = malloc(sizeof(BigNum));
  bn->ref = 1;
  bn->is_small = 0;
  bn->i = 0;
  bn->neg = neg;
  bn->nlimbs = nlimbs;
  if (nlimbs == 0) {
    bn->limbs = NULL;
  } else {
    bn->limbs = malloc(nlimbs * sizeof(uint32_t));
    memcpy(bn->limbs, limbs, nlimbs * sizeof(uint32_t));
  }
  return bn;
}
static void bignum_ref(BigNum *bn) { bn->ref++; }
/* Take a ref and return the pointer (mirrors Rc::clone). */
static BigNum *bignum_clone_ref(BigNum *bn) {
  bn->ref++;
  return bn;
}
static void bignum_unref(BigNum *bn) {
  if (!bn)
    return;
  if (--bn->ref == 0) {
    free(bn->limbs);
    free(bn);
  }
}
/* Wrap an i64 as a Big (signed-magnitude), for the overflow paths. Mirrors
 * i64_to_limbs: i64::MIN's magnitude is 2^63 = [0, 2^32]; 0 -> empty limbs. */
static BigNum *bignum_i64_to_big(int64_t i) {
  if (i == 0)
    return bignum_new_big(0, NULL, 0);
  if (i > 0) {
    uint32_t lo = (uint32_t)(uint64_t)i;
    uint32_t hi = (uint32_t)((uint64_t)i >> 32);
    if (hi == 0)
      return bignum_new_big(0, &lo, 1);
    uint32_t l[2] = {lo, hi};
    return bignum_new_big(0, l, 2);
  }
  uint64_t m = (uint64_t)(-(i + 1)) + 1; /* magnitude = |i| (safe for MIN) */
  uint32_t lo = (uint32_t)m;
  uint32_t hi = (uint32_t)(m >> 32);
  if (hi == 0)
    return bignum_new_big(1, &lo, 1);
  uint32_t l[2] = {lo, hi};
  return bignum_new_big(1, l, 2);
}
/* Build a BigNum from a signed magnitude, narrowing back to the Small fast
 * path when the magnitude fits i64. Mirrors Rust `from_mag`: zero -> Small(0);
 * >2 limbs -> Big (full magnitude kept); <=2 limbs -> Small when it fits
 * (positive: m <= i64::MAX; negative: m <= 2^63, incl. i64::MIN). */
static BigNum *bignum_from_mag(int neg, const uint32_t *mag, size_t n) {
  size_t len = n;
  while (len > 0 && mag[len - 1] == 0)
    len--;
  if (len == 0)
    return bignum_new_small(0);
  if (len > 2)
    return bignum_new_big(neg, mag, n);
  uint64_t lo = mag[0];
  uint64_t hi = (len >= 2) ? mag[1] : 0;
  uint64_t m = (hi << 32) | lo;
  if (!neg) {
    if (m <= (uint64_t)INT64_MAX)
      return bignum_new_small((int64_t)m);
  } else {
    if (m <= ((uint64_t)INT64_MAX) + 1) {
      int64_t val = (m == ((uint64_t)INT64_MAX) + 1) ? INT64_MIN
                                                     : -(int64_t)m;
      return bignum_new_small(val);
    }
  }
  return bignum_new_big(neg, mag, n);
}

/* Unsigned-magnitude add/sub/mul on normalized limb arrays. Mirrors
 * big_add / big_sub / big_mul. */
static uint32_t *big_add(const uint32_t *a, size_t na, const uint32_t *b,
                         size_t nb, size_t *out_n) {
  size_t n = na > nb ? na : nb;
  uint32_t *r = malloc(n * sizeof(uint32_t));
  uint64_t c = 0;
  for (size_t i = 0; i < n; i++) {
    uint64_t s = c;
    if (i < na)
      s += a[i];
    if (i < nb)
      s += b[i];
    r[i] = (uint32_t)s;
    c = s >> 32;
  }
  if (c) {
    uint32_t *r2 = malloc((n + 1) * sizeof(uint32_t));
    memcpy(r2, r, n * sizeof(uint32_t));
    r2[n] = (uint32_t)c;
    free(r);
    r = r2;
    n++;
  }
  while (n > 0 && r[n - 1] == 0)
    n--;
  if (n == 0) {
    free(r);
    r = NULL;
  } else {
    uint32_t *r2 = malloc(n * sizeof(uint32_t));
    memcpy(r2, r, n * sizeof(uint32_t));
    free(r);
    r = r2;
  }
  *out_n = n;
  return r;
}
/* Requires mag(a) >= mag(b). */
static uint32_t *big_sub(const uint32_t *a, size_t na, const uint32_t *b,
                         size_t nb, size_t *out_n) {
  uint32_t *r = malloc(na * sizeof(uint32_t));
  int64_t borrow = 0;
  for (size_t i = 0; i < na; i++) {
    int64_t av = (int64_t)a[i];
    int64_t bv = i < nb ? (int64_t)b[i] : 0;
    int64_t d = av - bv - borrow;
    if (d < 0) {
      d += (int64_t)1 << 32;
      borrow = 1;
    } else {
      borrow = 0;
    }
    r[i] = (uint32_t)d;
  }
  size_t n = na;
  while (n > 0 && r[n - 1] == 0)
    n--;
  if (n == 0) {
    free(r);
    r = NULL;
  } else {
    uint32_t *r2 = malloc(n * sizeof(uint32_t));
    memcpy(r2, r, n * sizeof(uint32_t));
    free(r);
    r = r2;
  }
  *out_n = n;
  return r;
}
static uint32_t *big_mul(const uint32_t *a, size_t na, const uint32_t *b,
                         size_t nb, size_t *out_n) {
  if (na == 0 || nb == 0) {
    *out_n = 0;
    return NULL;
  }
  size_t n = na + nb;
  uint64_t *r = calloc(n, sizeof(uint64_t));
  for (size_t i = 0; i < na; i++) {
    uint64_t carry = 0;
    for (size_t j = 0; j < nb; j++) {
      uint64_t cur = r[i + j] + (uint64_t)a[i] * b[j] + carry;
      r[i + j] = cur & 0xFFFFFFFFu;
      carry = cur >> 32;
    }
    size_t k = i + nb;
    while (carry) {
      uint64_t cur = r[k] + carry;
      r[k] = cur & 0xFFFFFFFFu;
      carry = cur >> 32;
      k++;
    }
  }
  size_t m = n;
  while (m > 0 && r[m - 1] == 0)
    m--;
  if (m == 0) {
    free(r);
    *out_n = 0;
    return NULL;
  }
  uint32_t *out = malloc(m * sizeof(uint32_t));
  for (size_t i = 0; i < m; i++)
    out[i] = (uint32_t)r[i];
  free(r);
  *out_n = m;
  return out;
}
/* Compare two magnitudes that may carry leading zero limbs. -1/0/1. */
static int mag_cmp(const uint32_t *a, size_t na, const uint32_t *b,
                   size_t nb) {
  while (na > 0 && a[na - 1] == 0)
    na--;
  while (nb > 0 && b[nb - 1] == 0)
    nb--;
  if (na != nb)
    return na < nb ? -1 : 1;
  for (size_t i = na; i > 0; i--) {
    if (a[i - 1] != b[i - 1])
      return a[i - 1] < b[i - 1] ? -1 : 1;
  }
  return 0;
}
static int mag_ge(const uint32_t *a, size_t na, const uint32_t *b, size_t nb) {
  return mag_cmp(a, na, b, nb) >= 0;
}
/* a -= b in place (a has `na` allocated limbs). Requires mag(a) >= mag(b). */
static void mag_sub_inplace(uint32_t *a, size_t na, const uint32_t *b,
                            size_t nb) {
  int64_t borrow = 0;
  for (size_t i = 0; i < na; i++) {
    int64_t av = (int64_t)a[i];
    int64_t bv = i < nb ? (int64_t)b[i] : 0;
    int64_t d = av - bv - borrow;
    if (d < 0) {
      d += (int64_t)1 << 32;
      borrow = 1;
    } else {
      borrow = 0;
    }
    a[i] = (uint32_t)d;
  }
}
/* Unsigned long division of magnitude a by magnitude b (b != 0): returns the
 * normalized remainder r = a mod b (0 <= r < b). Mirrors mag_divmod
 * (bit-by-bit shift-subtract); the quotient is not needed. Caller frees *out. */
static void mag_divmod_rem(const uint32_t *a, size_t na, const uint32_t *b,
                           size_t nb, uint32_t **out, size_t *out_n) {
  uint32_t *r = calloc(na + 1, sizeof(uint32_t));
  size_t total_bits = na * 32;
  for (size_t i = total_bits; i > 0; i--) {
    size_t bit = i - 1;
    uint32_t carry = 0;
    for (size_t limb = 0; limb <= na; limb++) {
      uint32_t new_carry = r[limb] >> 31;
      r[limb] = (r[limb] << 1) | carry;
      carry = new_carry;
    }
    r[0] |= (a[bit / 32] >> (bit % 32)) & 1;
    if (mag_ge(r, na + 1, b, nb))
      mag_sub_inplace(r, na + 1, b, nb);
  }
  size_t n = na + 1;
  while (n > 0 && r[n - 1] == 0)
    n--;
  if (n == 0) {
    free(r);
    *out = NULL;
    *out_n = 0;
  } else {
    uint32_t *r2 = malloc(n * sizeof(uint32_t));
    memcpy(r2, r, n * sizeof(uint32_t));
    free(r);
    *out = r2;
    *out_n = n;
  }
}
/* Signed-magnitude compare of two magnitudes (a vs b): -1/0/1. */
static int big_cmp_mag(const uint32_t *a, size_t na, const uint32_t *b,
                       size_t nb) {
  if (na != nb)
    return na < nb ? -1 : 1;
  for (size_t i = na; i > 0; i--) {
    if (a[i - 1] != b[i - 1])
      return a[i - 1] < b[i - 1] ? -1 : 1;
  }
  return 0;
}
/* Unsigned-magnitude to decimal (no sign). Mirrors big_to_string. */
static void big_to_string(const uint32_t *limbs, size_t n, char *buf,
                          size_t bufsz) {
  if (n == 0) {
    buf[0] = '0';
    buf[1] = 0;
    return;
  }
  uint32_t *tmp = malloc(n * sizeof(uint32_t));
  memcpy(tmp, limbs, n * sizeof(uint32_t));
  char *digits = malloc((n * 10 + 1) * sizeof(char));
  size_t nd = 0;
  while (n > 0) {
    uint64_t rem = 0;
    for (size_t i = n; i > 0; i--) {
      uint64_t cur = (rem << 32) | tmp[i - 1];
      tmp[i - 1] = (uint32_t)(cur / 10);
      rem = cur % 10;
    }
    digits[nd++] = (char)('0' + rem);
    while (n > 0 && tmp[n - 1] == 0)
      n--;
  }
  free(tmp);
  size_t need = nd + 1;
  if (need > bufsz)
    need = bufsz;
  for (size_t i = 0; i < nd && i + 1 < bufsz; i++)
    buf[i] = digits[nd - 1 - i];
  buf[need - 1] = 0;
  free(digits);
}
static int bignum_is_zero(BigNum *bn) {
  if (bn->is_small)
    return bn->i == 0;
  return bn->nlimbs == 0;
}
static int bignum_cmp(BigNum *a, BigNum *b); /* fwd (defined below) */

/* ---- BigNum arithmetic (mirrors the Rust impl) ------------------------- */
static BigNum *bignum_add(BigNum *a, BigNum *b) {
  if (a->is_small && b->is_small) {
    int64_t r;
    if (checked_add(a->i, b->i, &r))
      return bignum_new_small(r);
    BigNum *ab = bignum_i64_to_big(a->i);
    BigNum *bb = bignum_i64_to_big(b->i);
    BigNum *r2 = bignum_add(ab, bb);
    bignum_unref(ab);
    bignum_unref(bb);
    return r2;
  }
  if (a->is_small) {
    BigNum *ab = bignum_i64_to_big(a->i);
    BigNum *r = bignum_add(ab, b);
    bignum_unref(ab);
    return r;
  }
  if (b->is_small) {
    BigNum *bb = bignum_i64_to_big(b->i);
    BigNum *r = bignum_add(a, bb);
    bignum_unref(bb);
    return r;
  }
  /* Both Big. */
  if (a->neg == b->neg) {
    size_t n;
    uint32_t *m = big_add(a->limbs, a->nlimbs, b->limbs, b->nlimbs, &n);
    BigNum *out = bignum_from_mag(a->neg, m, n);
    free(m);
    return out;
  }
  int c = big_cmp_mag(a->limbs, a->nlimbs, b->limbs, b->nlimbs);
  if (c == 0)
    return bignum_new_small(0);
  if (c > 0) { /* |a| > |b|, result sign = a's */
    size_t n;
    uint32_t *m = big_sub(a->limbs, a->nlimbs, b->limbs, b->nlimbs, &n);
    BigNum *out = bignum_from_mag(a->neg, m, n);
    free(m);
    return out;
  }
  size_t n;
  uint32_t *m = big_sub(b->limbs, b->nlimbs, a->limbs, a->nlimbs, &n);
  BigNum *out = bignum_from_mag(b->neg, m, n);
  free(m);
  return out;
}
static BigNum *bignum_sub(BigNum *a, BigNum *b) {
  if (a->is_small && b->is_small) {
    int64_t r;
    if (checked_sub(a->i, b->i, &r))
      return bignum_new_small(r);
    BigNum *ab = bignum_i64_to_big(a->i);
    BigNum *bb = bignum_i64_to_big(b->i);
    BigNum *r2 = bignum_sub(ab, bb);
    bignum_unref(ab);
    bignum_unref(bb);
    return r2;
  }
  if (a->is_small) {
    BigNum *ab = bignum_i64_to_big(a->i);
    BigNum *r = bignum_sub(ab, b);
    bignum_unref(ab);
    return r;
  }
  if (b->is_small) {
    if (b->i == 0)
      return bignum_new_big(a->neg, a->limbs, a->nlimbs);
    BigNum *bb = bignum_i64_to_big(b->i);
    BigNum *r = bignum_sub(a, bb);
    bignum_unref(bb);
    return r;
  }
  /* Both Big. a - b. */
  if (a->neg == b->neg) { /* same sign: subtract magnitudes, sign = a's */
    int c = big_cmp_mag(a->limbs, a->nlimbs, b->limbs, b->nlimbs);
    if (c == 0)
      return bignum_new_small(0);
    if (c > 0) {
      size_t n;
      uint32_t *m = big_sub(a->limbs, a->nlimbs, b->limbs, b->nlimbs, &n);
      BigNum *out = bignum_from_mag(a->neg, m, n);
      free(m);
      return out;
    }
    size_t n;
    uint32_t *m = big_sub(b->limbs, b->nlimbs, a->limbs, a->nlimbs, &n);
    BigNum *out = bignum_from_mag(!a->neg, m, n);
    free(m);
    return out;
  }
  /* Opposite signs: a - b = a + (-b) => add magnitudes, sign = a's. */
  size_t n;
  uint32_t *m = big_add(a->limbs, a->nlimbs, b->limbs, b->nlimbs, &n);
  BigNum *out = bignum_from_mag(a->neg, m, n);
  free(m);
  return out;
}
static BigNum *bignum_mul(BigNum *a, BigNum *b) {
  if (a->is_small && b->is_small) {
    int64_t r;
    if (checked_mul(a->i, b->i, &r))
      return bignum_new_small(r);
    BigNum *ab = bignum_i64_to_big(a->i);
    BigNum *bb = bignum_i64_to_big(b->i);
    BigNum *r2 = bignum_mul(ab, bb);
    bignum_unref(ab);
    bignum_unref(bb);
    return r2;
  }
  if (a->is_small) {
    if (a->i == 0)
      return bignum_new_small(0);
    if (a->i == 1)
      return bignum_new_big(b->neg, b->limbs, b->nlimbs);
    BigNum *ab = bignum_i64_to_big(a->i);
    BigNum *r = bignum_mul(ab, b);
    bignum_unref(ab);
    return r;
  }
  if (b->is_small) {
    if (b->i == 0)
      return bignum_new_small(0);
    if (b->i == 1)
      return bignum_new_big(a->neg, a->limbs, a->nlimbs);
    BigNum *bb = bignum_i64_to_big(b->i);
    BigNum *r = bignum_mul(a, bb);
    bignum_unref(bb);
    return r;
  }
  int neg = a->neg != b->neg;
  size_t n;
  uint32_t *m = big_mul(a->limbs, a->nlimbs, b->limbs, b->nlimbs, &n);
  BigNum *out = bignum_from_mag(neg, m, n);
  free(m);
  return out;
}
static int bignum_cmp(BigNum *a, BigNum *b) {
  if (a->is_small && b->is_small) {
    uint64_t ua = (uint64_t)a->i, ub = (uint64_t)b->i;
    if (ua != ub)
      return ua < ub ? -1 : 1;
    return 0;
  }
  if (a->is_small) {
    BigNum *ab = bignum_i64_to_big(a->i);
    int c = bignum_cmp(ab, b);
    bignum_unref(ab);
    return c;
  }
  if (b->is_small) {
    BigNum *bb = bignum_i64_to_big(b->i);
    int c = bignum_cmp(a, bb);
    bignum_unref(bb);
    return c;
  }
  if (a->neg != b->neg)
    return a->neg ? -1 : 1;
  int c = big_cmp_mag(a->limbs, a->nlimbs, b->limbs, b->nlimbs);
  if (c == 0)
    return 0;
  return a->neg ? -c : c;
}
static BigNum *bignum_mod(BigNum *a, BigNum *b) {
  if (a->is_small && b->is_small) {
    int64_t x = a->i, y = b->i;
    if (y == 0)
      return NULL; /* caller sets "mod by zero" */
    if (y == -1)
      return bignum_new_small(0);
    int64_t r = x % y;
    if (r < 0)
      r += (y < 0) ? -y : y; /* Euclidean: result in [0, |y|) */
    return bignum_new_small(r);
  }
  BigNum *ab = a->is_small ? bignum_i64_to_big(a->i) : a;
  BigNum *bb = b->is_small ? bignum_i64_to_big(b->i) : b;
  /* Mirrors big_rem_euclid: r_mag = |a| mod |b|; if a is negative,
   * r_euclid = |b| - r_mag (exact division -> |b|, not 0). Always non-negative. */
  uint32_t *r_mag = NULL;
  size_t rn = 0;
  mag_divmod_rem(ab->limbs, ab->nlimbs, bb->limbs, bb->nlimbs, &r_mag, &rn);
  BigNum *out;
  if (ab->neg) {
    if (rn == 0) {
      /* |b| - 0 = |b| (the exact-division quirk). */
      out = bignum_from_mag(0, bb->limbs, bb->nlimbs);
    } else {
      uint32_t *bm = malloc(bb->nlimbs * sizeof(uint32_t));
      memcpy(bm, bb->limbs, bb->nlimbs * sizeof(uint32_t));
      mag_sub_inplace(bm, bb->nlimbs, r_mag, rn);
      out = bignum_from_mag(0, bm, bb->nlimbs);
      free(bm);
    }
  } else {
    out = bignum_from_mag(0, r_mag, rn);
  }
  free(r_mag);
  if (ab != a)
    bignum_unref(ab);
  if (bb != b)
    bignum_unref(bb);
  return out;
}
static BigNum *bignum_neg(BigNum *a) {
  if (a->is_small) {
    if (a->i == INT64_MIN)
      return bignum_new_big(0, (const uint32_t[]){0, 1}, 2); /* +2^63 */
    return bignum_new_small(-a->i);
  }
  /* Mirrors Rust: neg of a Big FLIPS the sign (magnitude shared). */
  return bignum_new_big(!a->neg, a->limbs, a->nlimbs);
}
static BigNum *bignum_abs(BigNum *a) {
  if (a->is_small) {
    if (a->i == INT64_MIN)
      return bignum_new_big(0, (const uint32_t[]){0, 1}, 2); /* +2^63 */
    return bignum_new_small(a->i < 0 ? -a->i : a->i);
  }
  /* Mirrors Rust: abs of a Big keeps the magnitude, sign cleared (positive). */
  return bignum_new_big(0, a->limbs, a->nlimbs);
}
static double bignum_to_f64(BigNum *bn) {
  if (bn->is_small)
    return (double)bn->i;
  double acc = 0.0;
  for (size_t i = bn->nlimbs; i > 0; i--)
    acc = acc * 4294967296.0 + (double)bn->limbs[i - 1];
  return bn->neg ? -acc : acc;
}
static void bignum_to_string(BigNum *bn, char *buf, size_t bufsz) {
  if (bn->is_small) {
    snprintf(buf, bufsz, "%lld", (long long)bn->i);
    return;
  }
  if (bn->nlimbs == 0) {
    buf[0] = '0';
    buf[1] = 0;
    return;
  }
  size_t cap = bn->nlimbs * 10 + 2;
  char *tmp = malloc(cap);
  big_to_string(bn->limbs, bn->nlimbs, tmp, cap);
  size_t len = strlen(tmp);
  size_t off = 0;
  if (bn->neg && bufsz > 1) {
    buf[0] = '-';
    off = 1;
  }
  size_t copy = len;
  if (off + copy >= bufsz)
    copy = (bufsz > off) ? bufsz - off - 1 : 0;
  memcpy(buf + off, tmp, copy);
  buf[off + copy] = 0;
  free(tmp);
}

/* ---- equality (matches the interpreter's PartialEq) -------------------- */
static int values_eq(Value *a, Value *b) {
  if (a->tag != b->tag) {
    /* int/float cross-compare (int-vs-int incl. V_BIGINT is exact, below). */
    if ((a->tag == V_INT && b->tag == V_FLOAT) ||
        (a->tag == V_FLOAT && b->tag == V_INT) ||
        (a->tag == V_BIGINT && b->tag == V_FLOAT) ||
        (a->tag == V_FLOAT && b->tag == V_BIGINT)) {
      double x = as_f64(a), y = as_f64(b);
      return x == y;
    }
    /* int vs bignum: exact, never through f64. */
    if ((a->tag == V_INT && b->tag == V_BIGINT) ||
        (a->tag == V_BIGINT && b->tag == V_INT)) {
      Value *ii = (a->tag == V_INT) ? a : b;
      Value *bb = (a->tag == V_INT) ? b : a;
      BigNum *t = bignum_i64_to_big(ii->u.i);
      int c = bignum_cmp(t, bb->u.bn);
      bignum_unref(t);
      return c == 0;
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
  case V_BIGINT:
    return bignum_cmp(a->u.bn, b->u.bn) == 0;
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
  case V_BIGINT:
    bignum_to_string(v->u.bn, buf, bufsz);
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
  /* Fast path: all-int accumulation with checked i64 arithmetic (no
   * allocation). The moment an operand overflows i64 — or a float shows up —
   * we fall through to the BigNum / f64 path below, which is exact. This is
   * what keeps the AOT compute loop at its pre-bignum speed: the common case
   * of small integers never touches the heap. */
  int64_t acc_i64 = is_mul ? 1 : 0;
  int fast = 1;
  for (int k = 0; fast && k < nargs; k++) {
    Value *v = &args[k];
    if (v->tag != V_INT) {
      fast = 0;
      break;
    }
    int64_t r;
    int ok = is_mul ? checked_mul(acc_i64, v->u.i, &r)
                    : checked_add(acc_i64, v->u.i, &r);
    if (!ok) {
      fast = 0;
      break;
    }
    acc_i64 = r;
  }
  if (fast)
    return v_int(acc_i64);

  BigNum *acc_i = bignum_new_small(is_mul ? 1 : 0);
  double acc_f = 0.0;
  int is_float = 0;
  for (int k = 0; k < nargs; k++) {
    Value *v = &args[k];
    if (v->tag == V_INT && !is_float) {
      BigNum *t = bignum_i64_to_big(v->u.i);
      BigNum *r = is_mul ? bignum_mul(acc_i, t) : bignum_add(acc_i, t);
      bignum_unref(acc_i);
      bignum_unref(t);
      acc_i = r;
    } else if (v->tag == V_BIGINT && !is_float) {
      BigNum *t = bignum_clone_ref(v->u.bn);
      BigNum *r = is_mul ? bignum_mul(acc_i, t) : bignum_add(acc_i, t);
      bignum_unref(acc_i);
      bignum_unref(t);
      acc_i = r;
    } else {
      double x = as_f64(v);
      if (g_err) {
        bignum_unref(acc_i);
        return v_nil();
      }
      if (!is_float) {
        is_float = 1;
        acc_f = bignum_to_f64(acc_i);
      }
      acc_f = is_mul ? acc_f * x : acc_f + x;
    }
  }
  Value out = is_float ? v_float(acc_f) : v_from_bignum(acc_i);
  return out;
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
      BigNum *t = bignum_i64_to_big(a->u.i);
      BigNum *r = bignum_neg(t);
      bignum_unref(t);
      return v_from_bignum(r);
    }
    if (a->tag == V_BIGINT)
      return v_from_bignum(bignum_neg(a->u.bn));
    if (a->tag == V_FLOAT)
      return v_float(-a->u.f);
    set_err("- expected number, got %s", type_name(a));
    return v_nil();
  }
  int all_int = (args[0].tag == V_INT || args[0].tag == V_BIGINT);
  for (int k = 1; k < nargs && all_int; k++)
    all_int = (args[k].tag == V_INT || args[k].tag == V_BIGINT);
  if (all_int) {
    BigNum *acc;
    if (args[0].tag == V_INT)
      acc = bignum_i64_to_big(args[0].u.i);
    else
      acc = bignum_clone_ref(args[0].u.bn);
    for (int k = 1; k < nargs; k++) {
      BigNum *t = (args[k].tag == V_INT) ? bignum_i64_to_big(args[k].u.i)
                                         : bignum_clone_ref(args[k].u.bn);
      BigNum *r = bignum_sub(acc, t);
      bignum_unref(acc);
      bignum_unref(t);
      acc = r;
    }
    return v_from_bignum(acc);
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
  if (nargs != 2 ||
      (args[0].tag != V_INT && args[0].tag != V_BIGINT) ||
      (args[1].tag != V_INT && args[1].tag != V_BIGINT)) {
    set_err("mod expects (mod int int)");
    return v_nil();
  }
  BigNum *a = (args[0].tag == V_INT) ? bignum_i64_to_big(args[0].u.i)
                                     : bignum_clone_ref(args[0].u.bn);
  BigNum *b = (args[1].tag == V_INT) ? bignum_i64_to_big(args[1].u.i)
                                     : bignum_clone_ref(args[1].u.bn);
  if (bignum_is_zero(b)) {
    set_err("mod by zero");
    bignum_unref(a);
    bignum_unref(b);
    return v_nil();
  }
  BigNum *r = bignum_mod(a, b);
  bignum_unref(a);
  bignum_unref(b);
  return v_from_bignum(r);
}

static Value builtin_eq(Value *args, int nargs) {
  for (int k = 0; k + 1 < nargs; k++) {
    if (!values_eq(&args[k], &args[k + 1]))
      return v_bool(0);
  }
  return v_bool(1);
}

static int num_is_int(Value *v) {
  return v->tag == V_INT || v->tag == V_BIGINT;
}
/* Exact integer ordering for int-vs-int (incl. V_BIGINT), never through f64. */
static int int_order(Value *a, Value *b) {
  if (a->tag == V_INT && b->tag == V_INT) {
    if (a->u.i < b->u.i)
      return -1;
    if (a->u.i > b->u.i)
      return 1;
    return 0;
  }
  BigNum *ta = (a->tag == V_INT) ? bignum_i64_to_big(a->u.i)
                                 : bignum_clone_ref(a->u.bn);
  BigNum *tb = (b->tag == V_INT) ? bignum_i64_to_big(b->u.i)
                                 : bignum_clone_ref(b->u.bn);
  int c = bignum_cmp(ta, tb);
  bignum_unref(ta);
  bignum_unref(tb);
  return c;
}
static Value builtin_cmp(Value *args, int nargs, int op) {
  for (int k = 0; k + 1 < nargs; k++) {
    int keep;
    if (num_is_int(&args[k]) && num_is_int(&args[k + 1])) {
      int c = int_order(&args[k], &args[k + 1]);
      switch (op) {
      case 0:
        keep = c < 0;
        break;
      case 1:
        keep = c > 0;
        break;
      case 2:
        keep = c <= 0;
        break;
      default:
        keep = c >= 0;
        break;
      }
    } else {
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
  if (nargs != 2 || args[0].tag != V_LIST || !num_is_int(&args[1])) {
    set_err("nth expects (nth list int)");
    return v_nil();
  }
  // A V_BIGINT index is out of i64 range: the interpreter's as_i64() -> None
  // reports "nth index out of range", not the type error.
  if (args[1].tag == V_BIGINT) {
    set_err("nth index out of range");
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

/* ---- caught-error value ------------------------------------------------ */
/* The value a `catch` clause binds to: the hash
 *   {"message" <str>, "kind" "runtime"}
 *
 * Built by hand rather than by calling `builtin_hash` so the key ORDER is fixed
 * by construction. Every backend prints a map in insertion order, so
 * `message` before `kind` is what makes the caught value byte-identical across
 * the tree-walk, the VM, this runtime and the three transpilers.
 *
 * It also CLEARS g_err. That is the whole of the AOT unwind: the error has
 * already been recorded in g_errmsg, and a `catch` consumes it, so the code
 * after the dispatch must not see the flag. A generated `try` therefore calls
 * v_error_value() immediately after the failing body, before running the
 * handler. Spelled in prose rather than as a nested C comment because an
 * embedded comment terminator closes this block comment early, and the stray
 * text that follows is a -Wcomment warning on every single AOT compile.
 *
 * `kind` is always "runtime" here, and that is not a shortcut: the only errors
 * this runtime raises at run time are runtime errors, so the constant is what
 * the variant-derived value would be. (A lex or parse error stops the program
 * before any code runs, so a `catch` can never observe one.) */
static Value v_error_value(void) {
  Value r;
  r.tag = V_MAP;
  Map *m = malloc(sizeof(Map));
  m->ref = 1;
  m->n = 2;
  m->keys = malloc(2 * sizeof(Value));
  m->vals = malloc(2 * sizeof(Value));
  m->keys[0] = v_str("message");
  m->vals[0] = v_str(g_errmsg);
  m->keys[1] = v_str("kind");
  m->vals[1] = v_str("runtime");
  r.u.m = m;
  g_err = 0;
  return r;
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
  if (v->tag == V_BIGINT) {
    *out = bignum_to_f64(v->u.bn);
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

/* ---- Tier 3 file system -------------------------------------------------
 * Byte-for-byte twins of the five builtins in ainl-core/src/eval.rs,
 * including every error string and every decision about which syscall to make.
 * The rules are pinned in docs/SYNTAX.md §3h rather than delegated to the
 * host, for the same reason the path trio above is: the hosts disagree on
 * every edge case that matters. The two sharpest are an existing rename
 * destination (os.rename and File.rename clobber it silently; rename(2) does
 * not) and a missing parent directory (mkdir's arity differs in all three
 * hosts). */

#include <errno.h>
#include <sys/stat.h>
#include <sys/types.h>

/* EXDEV's value, spelled out rather than taken from <errno.h>.
 *
 * The interpreter cannot include a libc header either, so it hardcodes the
 * same 18 (see the EXDEV constant in eval.rs). POSIX fixes this number in its
 * error ABI and it is 18 on every platform AINL targets, which is what lets the
 * two backends name the same case without sharing a header. */
#define AINL_EXDEV 18

/* The path a filesystem *query* builtin probes, with a trailing separator
 * stripped — the same helper the interpreter's `fs_probe_path` describes. The
 * hosts split on whether the trailing form is legal against a non-directory
 * (ENOTDIR on Linux, NotADirectoryError in Python, a throw in Node,
 * Errno::ENOTDIR in Ruby), so it is trimmed here.
 *
 * Returns a malloc'd string the caller frees, or NULL on allocation failure
 * (which the runtime cannot report any better than "out of memory"). The
 * common no-strip case still allocates, so every caller has exactly one free. */
static char *fs_probe_path(const char *path) {
  size_t n = strlen(path);
  /* A lone "/" is the root and must survive, so the strip is skipped when it
   * would empty the string. */
  if (n > 1 && path[n - 1] == '/') {
    char *trimmed = malloc(n);
    if (!trimmed)
      return NULL;
    memcpy(trimmed, path, n - 1);
    trimmed[n - 1] = 0;
    return trimmed;
  }
  char *copy = malloc(n + 1);
  if (!copy)
    return NULL;
  memcpy(copy, path, n + 1);
  return copy;
}

/* mkdir, with the option as a positional string rather than a keyword — AINL
 * has no keyword-argument syntax, so ":recursive" arrives as an ordinary
 * string. See docs/SYNTAX.md §3h.
 *
 * An existing path is an error in *both* modes, checked before the host call
 * so the message is the same everywhere. The hosts do agree that an existing
 * directory is a failure, but they express it differently (EEXIST, Errno::EEXIST,
 * and Python's FileExistsError) and this keeps the text identical.
 *
 * The recursive case is a hand-rolled create_dir_all: the C runtime has no
 * mkdir -p, and every failure collapses to the same "cannot create" message
 * because AINL does not surface errno (see the same comment in eval.rs). */
static Value builtin_mkdir(Value *args, int nargs) {
  if (nargs < 1 || nargs > 2) {
    set_err("mkdir expects (mkdir path) or (mkdir path option)");
    return v_nil();
  }
  const char *path = as_path_arg(&args[0], "mkdir");
  if (!path)
    return v_nil();
  int recursive = 0;
  if (nargs == 2) {
    if (args[1].tag != V_STR) {
      set_err("mkdir expects a str option, got %s", type_name(&args[1]));
      return v_nil();
    }
    if (strcmp(args[1].u.s->data, ":recursive") != 0) {
      set_err("mkdir: unknown option '%s'", args[1].u.s->data);
      return v_nil();
    }
    recursive = 1;
  }
  char *probe = fs_probe_path(path);
  if (!probe) {
    set_err("mkdir: out of memory");
    return v_nil();
  }
  struct stat st;
  int exists = lstat(probe, &st) == 0;
  free(probe);
  if (exists) {
    set_err("mkdir: cannot create '%s': it exists", path);
    return v_nil();
  }
  if (!recursive) {
    if (mkdir(path, 0777) != 0) {
      set_err("mkdir: cannot create '%s'", path);
    }
    return v_nil();
  }
  /* Recursive: create each missing ancestor in turn, keeping the caller's
   * spelling. A leading "/" is preserved, so the first component is the empty
   * string and the loop starts at the separator. A ".." segment is created as
   * a literal directory name rather than resolved — AINL never resolves ".."
   * (see the path-* trio), and mkdir(2) on a path containing it is an error
   * anyway, so a caller who wrote one gets the honest "cannot create". */
  size_t len = strlen(path);
  for (size_t i = 1; i <= len; i++) {
    if (i != len && path[i] != '/')
      continue;
    if (i == 0)
      continue;
    char *prefix = malloc(i + 1);
    if (!prefix) {
      set_err("mkdir: out of memory");
      return v_nil();
    }
    memcpy(prefix, path, i);
    prefix[i] = 0;
    /* Skip an empty prefix (a leading "/" on its own is the root, which
     * always exists) and anything that already exists. */
    if (prefix[0] != 0 && lstat(prefix, &st) != 0)
      mkdir(prefix, 0777);
    free(prefix);
  }
  /* The loop is best-effort, so success is decided by looking again: a
   * concurrent creator, a permission failure on a parent, and a ".."
   * component must all report the same error rather than a silent nil. */
  char *final_probe = fs_probe_path(path);
  if (!final_probe) {
    set_err("mkdir: out of memory");
    return v_nil();
  }
  int ok = lstat(final_probe, &st) == 0 && S_ISDIR(st.st_mode);
  free(final_probe);
  if (!ok)
    set_err("mkdir: cannot create '%s'", path);
  return v_nil();
}

/* (rename from to) -> nil. A single rename(2): atomic, and it moves a whole
 * subtree without reading it, which is the whole point over read->write->
 * delete.
 *
 * Both pre-checks happen before the syscall because the hosts disagree about
 * an existing destination: os.rename and File.rename clobber it silently,
 * while rename(2) only fails for a non-empty directory target. Refusing here
 * is what stops a compiled AINL program from silently destroying a file that
 * the same program running under the interpreter would have preserved.
 *
 * EXDEV is named rather than collapsed into the generic message, because
 * "different filesystems" and "not writable" need different fixes and the
 * caller can only tell them apart from the text. There is no copy-and-delete
 * fallback: it would change the contract from atomic to not and would be a
 * different algorithm per backend. `copy` is there for the caller who wants
 * the data movement. */
static Value builtin_rename(Value *args, int nargs) {
  if (nargs != 2) {
    set_err("rename expects (rename from to)");
    return v_nil();
  }
  const char *from = as_path_arg(&args[0], "rename");
  if (!from)
    return v_nil();
  const char *to = as_path_arg(&args[1], "rename");
  if (!to)
    return v_nil();
  char *pfrom = fs_probe_path(from);
  if (!pfrom) {
    set_err("rename: out of memory");
    return v_nil();
  }
  char *pto = fs_probe_path(to);
  if (!pto) {
    free(pfrom);
    set_err("rename: out of memory");
    return v_nil();
  }
  struct stat st;
  if (lstat(pfrom, &st) != 0) {
    set_err("rename: cannot move '%s': it does not exist", from);
    free(pfrom);
    free(pto);
    return v_nil();
  }
  if (lstat(pto, &st) == 0) {
    set_err("rename: cannot move '%s': '%s' exists", from, to);
    free(pfrom);
    free(pto);
    return v_nil();
  }
  /* The syscall gets the caller's spelling, not the trimmed probe: renaming
   * "a/b/" is legal POSIX and the trailing separator is part of the name the
   * caller wrote. Only the *queries* above needed trimming. */
  if (rename(from, to) != 0) {
    if (errno == AINL_EXDEV)
      set_err("rename: cannot move '%s' to '%s': different filesystems", from, to);
    else
      set_err("rename: cannot move '%s' to '%s'", from, to);
  }
  free(pfrom);
  free(pto);
  return v_nil();
}

/* (copy from to) -> nil. A byte-for-byte copy of one file: read and write the
 * contents, never link(2), so the two files diverge afterwards. A hardlink
 * would share the inode, and a later write-file on either path would silently
 * change both — a worse surprise than the I/O. For a large file this is a full
 * read + write, which is the documented cost.
 *
 * "rb"/"wb" so no CRLF translation can change the bytes on a Windows host,
 * matching read-file's mode. A directory source is refused rather than
 * silently skipped: there is no copy -r and no delete-dir to pair it with. */
static Value builtin_copy(Value *args, int nargs) {
  if (nargs != 2) {
    set_err("copy expects (copy from to)");
    return v_nil();
  }
  const char *from = as_path_arg(&args[0], "copy");
  if (!from)
    return v_nil();
  const char *to = as_path_arg(&args[1], "copy");
  if (!to)
    return v_nil();
  char *probe = fs_probe_path(from);
  if (!probe) {
    set_err("copy: out of memory");
    return v_nil();
  }
  struct stat st;
  if (lstat(probe, &st) != 0) {
    set_err("copy: cannot copy '%s': it does not exist", from);
    free(probe);
    return v_nil();
  }
  if (S_ISDIR(st.st_mode)) {
    set_err("copy: cannot copy '%s': it is a directory", from);
    free(probe);
    return v_nil();
  }
  FILE *in = fopen(probe, "rb");
  if (!in) {
    set_err("copy: cannot copy '%s' to '%s'", from, to);
    free(probe);
    return v_nil();
  }
  /* "wb" truncates an existing destination, exactly as write-file does. */
  FILE *out = fopen(to, "wb");
  if (!out) {
    fclose(in);
    set_err("copy: cannot copy '%s' to '%s'", from, to);
    free(probe);
    return v_nil();
  }
  char buf[8192];
  size_t n;
  int bad = 0;
  while ((n = fread(buf, 1, sizeof(buf), in)) > 0) {
    if (fwrite(buf, 1, n, out) != n) {
      bad = 1;
      break;
    }
  }
  if (ferror(in) || fflush(out) != 0)
    bad = 1;
  fclose(in);
  if (fclose(out) != 0)
    bad = 1;
  free(probe);
  if (bad)
    set_err("copy: cannot copy '%s' to '%s'", from, to);
  return v_nil();
}

/* (is-dir p) -> true, or nil. lstat, not stat: a symlink to a directory is a
 * symlink, so the link itself is not a directory. This is the real primitive
 * the old (file-exists (path-join p ".")) trick was reaching for, and it
 * returns nil (not false) so that (= (is-dir p) nil) is the absence test every
 * AINL program already writes — the same choice file-exists makes. */
static Value builtin_is_dir(Value *args, int nargs) {
  if (nargs != 1) {
    set_err("is-dir expects (is-dir path)");
    return v_nil();
  }
  const char *path = as_path_arg(&args[0], "is-dir");
  if (!path)
    return v_nil();
  char *probe = fs_probe_path(path);
  if (!probe) {
    set_err("is-dir: out of memory");
    return v_nil();
  }
  struct stat st;
  int ok = lstat(probe, &st) == 0 && S_ISDIR(st.st_mode);
  free(probe);
  return ok ? v_bool(1) : v_nil();
}

/* (file-size p) -> int, the size in *bytes*. Bytes, not characters: a file is
 * a sequence of bytes and there is no encoding in the file, which is what
 * makes this the companion to the byte-indexed string primitives.
 *
 * A directory is an error, not a number: POSIX reports the directory's own
 * inode size (4096 on ext4, 60 on APFS, 0 on tmpfs), so answering would report
 * a filesystem implementation detail as a language value. A missing path is
 * the same "cannot read" read-file raises, so sizing a typo does not look like
 * a zero-byte file. */
static Value builtin_file_size(Value *args, int nargs) {
  if (nargs != 1) {
    set_err("file-size expects (file-size path)");
    return v_nil();
  }
  const char *path = as_path_arg(&args[0], "file-size");
  if (!path)
    return v_nil();
  char *probe = fs_probe_path(path);
  if (!probe) {
    set_err("file-size: out of memory");
    return v_nil();
  }
  struct stat st;
  if (lstat(probe, &st) != 0) {
    set_err("file-size: cannot read '%s'", path);
    free(probe);
    return v_nil();
  }
  if (S_ISDIR(st.st_mode)) {
    set_err("file-size: cannot read '%s': it is a directory", path);
    free(probe);
    return v_nil();
  }
  off_t size = st.st_size;
  free(probe);
  return v_int((int64_t)size);
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

/* ---- Byte-oriented string primitives (Tier 3) ---------------------------
 *
 * AINL strings are BYTE strings: `substring`, `char`, `code` and `index-of`
 * index and slice by byte offset. Every host indexes its own way — Python's
 * and Ruby's String#index return a *character* offset, JavaScript's indexOf a
 * UTF-16 *code-unit* offset — so a natural strstr/memcpy port would disagree
 * with the interpreter on any non-ASCII input. The C runtime is the easiest
 * backend to get right by accident here precisely because strstr returns a
 * char*, which is already a byte pointer; the boundary checks below are what
 * keep it honest.
 *
 * Two rules are shared with the other four backends:
 *   * a slice that would split a multi-byte character is an error, not a
 *     replacement character (see utf8_valid for why there is no value to
 *     return);
 *   * `index-of` of an empty needle is 0, matching strstr's own answer.
 */

/* Is `at` a UTF-8 character boundary? A lead byte or the end of the string
 * starts a character; a continuation byte (10xxxxxx) does not. Every AINL
 * string in this runtime has already passed utf8_valid, so there is no
 * malformed case to decide here. */
static int a_is_char_boundary(const char *s, size_t at) {
  if (at == 0)
    return 1;
  unsigned char c = (unsigned char)s[at];
  return (c & 0xC0) != 0x80;
}

/* Is `v` an int index operand? On failure the message is AINL's own, matching
 * `as_index_arg` in ainl-core's eval.rs, and 0 is returned. Callers check
 * g_err, so no sentinel value is needed. */
static int a_is_index(Value *v, const char *who, const char *which) {
  if (v->tag != V_INT) {
    if (which && *which)
      set_err("%s expects an int %s index, got %s", who, which, type_name(v));
    else
      set_err("%s expects an int index, got %s", who, type_name(v));
    return 0;
  }
  return 1;
}

/* Resolve a byte offset, rejecting out-of-range and mid-character positions.
 * On failure sets the error and returns 0; callers test g_err. */
static size_t a_byte_offset(const char *s, size_t len, Value *iv, const char *who,
                            const char *which) {
  if (!a_is_index(iv, who, which))
    return 0;
  long long idx = iv->u.i;
  if (idx < 0 || idx > (long long)len) {
    set_err("%s %s index out of bounds", who, which);
    return 0;
  }
  if (!a_is_char_boundary(s, (size_t)idx)) {
    set_err("%s %s index splits a multi-byte character", who, which);
    return 0;
  }
  return (size_t)idx;
}

/* (substring s start end) -> the bytes in [start, end), end exclusive. */
static Value builtin_substring(Value *args, int nargs) {
  if (nargs != 3) {
    set_err("substring expects (substring str start end)");
    return v_nil();
  }
  const char *s = as_str_arg(&args[0], "substring");
  if (!s)
    return v_nil();
  size_t len = strlen(s);
  /* Type-check both bounds before reading either, so `(substring s 1.5 x)`
   * names the start index rather than a later failure. */
  if (!a_is_index(&args[1], "substring", "start"))
    return v_nil();
  if (!a_is_index(&args[2], "substring", "end"))
    return v_nil();
  if (args[1].u.i > args[2].u.i) {
    set_err("substring start index is greater than end index");
    return v_nil();
  }
  size_t lo = a_byte_offset(s, len, &args[1], "substring", "start");
  if (g_err)
    return v_nil();
  size_t hi = a_byte_offset(s, len, &args[2], "substring", "end");
  if (g_err)
    return v_nil();
  size_t n = hi - lo;
  char *out = malloc(n + 1);
  memcpy(out, s + lo, n);
  out[n] = 0;
  return v_str_take(out);
}

/* (char s i) -> the character starting at byte offset i, whole (never half). */
static Value builtin_char(Value *args, int nargs) {
  if (nargs != 2) {
    set_err("char expects (char str i)");
    return v_nil();
  }
  const char *s = as_str_arg(&args[0], "char");
  if (!s)
    return v_nil();
  size_t len = strlen(s);
  if (!a_is_index(&args[1], "char", NULL))
    return v_nil();
  long long i = args[1].u.i;
  if (i < 0 || (size_t)i >= len) {
    set_err("char index out of bounds");
    return v_nil();
  }
  if (!a_is_char_boundary(s, (size_t)i)) {
    set_err("char index splits a multi-byte character");
    return v_nil();
  }
  size_t n = 1;
  unsigned char c = (unsigned char)s[i];
  if (c >= 0xF0)
    n = 4;
  else if (c >= 0xE0)
    n = 3;
  else if (c >= 0xC0)
    n = 2;
  char *out = malloc(n + 1);
  memcpy(out, s + i, n);
  out[n] = 0;
  return v_str_take(out);
}

/* (code s) -> first byte value; (code s i) -> byte value at i. */
static Value builtin_code(Value *args, int nargs) {
  if (nargs != 1 && nargs != 2) {
    set_err("code expects (code str) or (code str i)");
    return v_nil();
  }
  const char *s = as_str_arg(&args[0], "code");
  if (!s)
    return v_nil();
  size_t len = strlen(s);
  if (nargs == 1) {
    if (len == 0) {
      set_err("code expects a non-empty string");
      return v_nil();
    }
    return v_int((long long)(unsigned char)s[0]);
  }
  if (!a_is_index(&args[1], "code", NULL))
    return v_nil();
  long long i = args[1].u.i;
  if (i < 0 || (size_t)i >= len) {
    set_err("code index out of bounds");
    return v_nil();
  }
  return v_int((long long)(unsigned char)s[i]);
}

/* (starts-with s prefix) -> bool. An empty prefix is true. */
static Value builtin_starts_with(Value *args, int nargs) {
  if (nargs != 2) {
    set_err("starts-with expects (starts-with str prefix)");
    return v_nil();
  }
  const char *s = as_str_arg(&args[0], "starts-with");
  if (!s)
    return v_nil();
  const char *pre = as_str_arg(&args[1], "starts-with");
  if (!pre)
    return v_nil();
  return v_bool(strncmp(s, pre, strlen(pre)) == 0);
}

/* (ends-with s suffix) -> bool. An empty suffix is true. */
static Value builtin_ends_with(Value *args, int nargs) {
  if (nargs != 2) {
    set_err("ends-with expects (ends-with str suffix)");
    return v_nil();
  }
  const char *s = as_str_arg(&args[0], "ends-with");
  if (!s)
    return v_nil();
  const char *suf = as_str_arg(&args[1], "ends-with");
  if (!suf)
    return v_nil();
  size_t slen = strlen(s), suflen = strlen(suf);
  return v_bool(slen >= suflen && memcmp(s + slen - suflen, suf, suflen) == 0);
}

/* (index-of s sub) -> byte index of the first occurrence, or -1. strstr
 * already returns a byte offset and already answers 0 for an empty needle. */
static Value builtin_index_of(Value *args, int nargs) {
  if (nargs != 2) {
    set_err("index-of expects (index-of str sub)");
    return v_nil();
  }
  const char *s = as_str_arg(&args[0], "index-of");
  if (!s)
    return v_nil();
  const char *sub = as_str_arg(&args[1], "index-of");
  if (!sub)
    return v_nil();
  const char *at = strstr(s, sub);
  return v_int(at ? (long long)(at - s) : -1);
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

/* (exit code) -> int. A V_BIGINT code is out of i64 range; the interpreter
 * clamps it to i32::MAX (as_i64() -> None -> i32::MAX), so this does too.
 * Does not return. stdout is flushed first: exit() does not flush stdio the
 * way a normal return does on every platform. */
static Value builtin_exit(Value *args, int nargs) {
  if (nargs != 1) {
    set_err("exit expects (exit code)");
    return v_nil();
  }
  if (!num_is_int(&args[0])) {
    set_err("exit expects an int, got %s", type_name(&args[0]));
    return v_nil();
  }
  int64_t code = (args[0].tag == V_BIGINT) ? (int64_t)INT32_MAX
                                          : args[0].u.i;
  fflush(stdout);
  fflush(stderr);
  exit((int)code);
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

/* (abs n) — integer-preserving and exact: abs(INT64_MIN) has no i64 answer, so
 * it widens to a bignum (9223372036854775808) exactly as the interpreter's
 * BigNum::abs does. A V_BIGINT argument is already out of i64 range and its
 * absolute value is the same magnitude with the sign cleared. */
static Value builtin_abs(Value *args, int nargs) {
  if (nargs != 1) {
    set_err("abs expects (abs n)");
    return v_nil();
  }
  Value *a = &args[0];
  if (a->tag == V_INT) {
    BigNum *t = bignum_i64_to_big(a->u.i);
    BigNum *r = bignum_abs(t);
    bignum_unref(t);
    return v_from_bignum(r);
  }
  if (a->tag == V_BIGINT)
    return v_bigint(bignum_abs(a->u.bn));
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
 * a large i64 or an out-of-range bignum keeps every digit). */
static Value builtin_floor(Value *args, int nargs) {
  if (nargs != 1) {
    set_err("floor expects (floor n)");
    return v_nil();
  }
  if (args[0].tag == V_INT)
    return v_int(args[0].u.i);
  if (args[0].tag == V_BIGINT)
    return v_bigint(bignum_clone_ref(args[0].u.bn));
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

/* Forward declarations: v_call's dispatch switch sits above the JSON
 * implementation, which is grouped with the other Tier 1 code further down. */
static Value builtin_json_parse(Value *args, int nargs);
static Value builtin_json_serialize(Value *args, int nargs);
static Value builtin_test(Value *args, int nargs);
static Value builtin_sort(Value *args, int nargs);

/* ---- Tier 4 storage: the AINL database ---------------------------------- *
 *
 * A hand-port of ainl-core/src/db.rs, which is the normative definition. Read
 * the two side by side: every rule below has a counterpart there, and the
 * reason the port exists at all is written in that file's header. The short
 * version: the engine is a 16-byte header followed by an append-only log, each
 * record is `key_len u32 | val_len u32 | crc32 u32 | key | value`, and `open`
 * replays it — discarding a torn or corrupt tail and truncating the file back
 * to the last good record, so the next append lands on a clean boundary.
 *
 * The CRC is the standard reflected IEEE 0xEDB88320, and the table below is
 * checked against zlib's own test vector (crc32("123456789") == 0xCBF43926) in
 * a unit test — a "checksum" that only round-trips between this file and the
 * Rust engine would not be a crash-recovery mechanism at all.
 *
 * libc only: open/read/write/ftruncate/fsync and the existing utf8_valid. No
 * dependency is added, so the `aot-standalone` job's musl-static link still
 * proves the property that the whole project sells.
 */
#define AINL_DB_MAGIC "AINLDB"
#define AINL_DB_VERSION 1
#define AINL_DB_HEADER_LEN 16
#define AINL_DB_REC_HEADER_LEN 12
/* Must match MAX_OPEN in ainl-core/src/db.rs: a program that opens the 65th
 * database has to be refused identically on both sides. */
#define AINL_DB_MAX_OPEN 64

/* Bucket count for the in-memory index. A prime so a key set of consecutive
 * integers (or of zero-padded names) does not pile into one chain — the
 * classic way a "hash table" ends up O(n) per lookup in practice. */
#define DB_BUCKETS 257

typedef struct DbEntry DbEntry;
struct DbEntry {
  char *key;
  char *val;
  DbEntry *next;
};

typedef struct Db Db;
/* Defined in the table-layer block further down, before it is first used. The
 * forward declaration is what lets `struct Db` own a table set: a handle and its
 * index then have exactly one lifetime, and `db_close_raw` cannot forget to free
 * one without also forgetting the other. */
struct DbtSet;

struct Db {
  char *path;  /* owned; only for error messages */
  FILE *f;     /* positioned at the end, ready to append */
  DbEntry *buckets[DB_BUCKETS];
  /* The table set, or NULL until a table builtin is first called on this
   * handle. Built lazily so a program that only ever calls `db-set` never pays
   * for a tree, and so the cost is paid once per handle rather than per open. */
  struct DbtSet *tables;
  int nkeys; /* records indexed, not distinct keys: an overwritten key counts
             * twice. Nothing reads it yet, but it is the number a future
             * compaction would need and it costs one int. */
};

/* Open handles, indexed by handle number minus one. A fixed table rather than
 * a growing array, for the same reason MAX_OPEN is: the generated binary
 * links against libc alone and has no allocator accounting to grow from. */
static Db *g_dbs[AINL_DB_MAX_OPEN];

/* CRC-32 table for the reflected IEEE polynomial, built at compile time. The
 * same construction as CRC_TABLE in db.rs — shift right, conditionally XOR
 * 0xEDB88320 — so the two cannot drift in *form*, and the test vector pins
 * that they do not drift in *value*. */
static uint32_t db_crc_table[256];
static void db_crc_init(void) {
  static int done = 0;
  if (done)
    return;
  done = 1;
  for (uint32_t i = 0; i < 256; i++) {
    uint32_t c = i;
    for (int k = 0; k < 8; k++)
      c = (c & 1) ? (0xEDB88320u ^ (c >> 1)) : (c >> 1);
    db_crc_table[i] = c;
  }
}

/* The CRC of the key followed by the value — the record's body, not its
 * header. Matches crc32_body in db.rs exactly. */
static uint32_t db_crc(const unsigned char *key, size_t klen, const unsigned char *val,
                       size_t vlen) {
  uint32_t c = 0xFFFFFFFFu;
  for (size_t i = 0; i < klen; i++)
    c = db_crc_table[(c ^ key[i]) & 0xFF] ^ (c >> 8);
  for (size_t i = 0; i < vlen; i++)
    c = db_crc_table[(c ^ val[i]) & 0xFF] ^ (c >> 8);
  return c ^ 0xFFFFFFFFu;
}

static uint32_t db_rd32(const unsigned char *b) {
  return (uint32_t)b[0] | ((uint32_t)b[1] << 8) | ((uint32_t)b[2] << 16) |
         ((uint32_t)b[3] << 24);
}

static void db_wr32(unsigned char *b, uint32_t v) {
  b[0] = (unsigned char)(v & 0xFF);
  b[1] = (unsigned char)((v >> 8) & 0xFF);
  b[2] = (unsigned char)((v >> 16) & 0xFF);
  b[3] = (unsigned char)((v >> 24) & 0xFF);
}

static void db_entry_free(Db *db) {
  for (int b = 0; b < DB_BUCKETS; b++) {
    DbEntry *e = db->buckets[b];
    while (e) {
      DbEntry *next = e->next;
      free(e->key);
      free(e->val);
      free(e);
      e = next;
    }
    db->buckets[b] = NULL;
  }
  db->nkeys = 0;
}

/* Defined in the table-layer block below, which is *after* this point in the
 * file. Declared here because `db_close_raw` — the single teardown path for a
 * handle — has to free the table set, so the declaration has to precede it. The
 * tag form (`struct DbtSet *`) is what makes that legal before the typedef
 * exists. */
static void dbt_set_free(struct DbtSet *s);

static void db_close_raw(Db *db) {
  if (!db)
    return;
  /* The table set hangs off the handle, so it is freed here rather than in
   * `db-close`: one teardown path means a close cannot leak a tree, and a
   * failed open cannot leave one behind either. `dbt_set_free` tolerates NULL. */
  if (db->tables)
    dbt_set_free(db->tables);
  db_entry_free(db);
  if (db->f)
    fclose(db->f);
  free(db->path);
  free(db);
}

/* A key is usable if it is well-formed UTF-8 with no NUL. The NUL rule is not
 * cosmetic: the value travels as a `char *` through the whole C runtime, so an
 * embedded NUL would truncate it here while the Rust engine kept it. Both sides
 * drop such a record so the same file recovers identically on each. */
static int db_usable(const unsigned char *b, size_t len) {
  if (!utf8_valid((const char *)b, len))
    return 0;
  return memchr(b, 0, len) == NULL;
}

/* The in-memory index: a chained hash table over the key bytes.
 *
 * Chaining, and newest-first within a bucket, is what makes "last write wins"
 * fall out for free — a lookup that finds a duplicate key stops at the most
 * recent record for it, which is exactly the answer replay-in-order gives. A
 * map that kept one entry per key would have to *replace* the value on every
 * put instead, which is the same amount of code for a worse failure mode: an
 * index that can only lose the older of two duplicates cannot report them at
 * all, and a program that reads back a key it overwrote gets the wrong answer
 * with no way to tell. The bucket count is a prime, so a key set of
 * consecutive integers does not pile into one chain — see DB_BUCKETS above.
 */

/* FNV-1a over the key. A different hash from the CRC on purpose: the CRC is
 * the *format's* integrity check and must never change, while this one only
 * picks a bucket, and the simplest thing that spreads bytes well is the right
 * amount of machinery. */
static uint32_t db_hash(const char *s) {
  uint32_t h = 2166136261u;
  for (const unsigned char *p = (const unsigned char *)s; *p; p++) {
    h ^= *p;
    h *= 16777619u;
  }
  return h;
}

static void db_index_put(Db *db, const char *key, const char *val) {
  DbEntry *e = malloc(sizeof(DbEntry));
  if (!e) {
    set_err("db-open: out of memory");
    return;
  }
  e->key = strdup(key);
  e->val = strdup(val);
  if (!e->key || !e->val) {
    free(e->key);
    free(e->val);
    free(e);
    set_err("db-open: out of memory");
    return;
  }
  DbEntry **slot = &db->buckets[db_hash(key) % DB_BUCKETS];
  e->next = *slot;
  *slot = e;
  db->nkeys++;
}

static const char *db_index_get(Db *db, const char *key) {
  for (DbEntry *e = db->buckets[db_hash(key) % DB_BUCKETS]; e; e = e->next)
    if (strcmp(e->key, key) == 0)
      return e->val;
  return NULL;
}

/* Replay the log into the index, then truncate any torn tail. Returns 0 and
 * sets g_err on a refusal (foreign file, wrong version); a torn *tail* is not
 * an error — it is the case this function exists to handle. */
static int db_replay(Db *db) {
  db_crc_init();
  long size;
  if (fseek(db->f, 0, SEEK_END) != 0)
    return 0;
  size = ftell(db->f);
  if (size < 0)
    return 0;
  if (size == 0) {
    /* A new file: write the header and there is nothing to replay. */
    unsigned char h[AINL_DB_HEADER_LEN];
    memset(h, 0, sizeof(h));
    memcpy(h, AINL_DB_MAGIC, 6);
    h[6] = AINL_DB_VERSION;
    db_wr32(h + 8, AINL_DB_HEADER_LEN);
    rewind(db->f);
    if (fwrite(h, 1, sizeof(h), db->f) != sizeof(h))
      return 0;
    fflush(db->f);
    return 1;
  }
  if (size < AINL_DB_HEADER_LEN) {
    set_err("db-open: '%s' is not an AINL database", db->path);
    return 0;
  }
  unsigned char *raw = malloc((size_t)size);
  if (!raw) {
    set_err("db-open: out of memory");
    return 0;
  }
  rewind(db->f);
  size_t got = fread(raw, 1, (size_t)size, db->f);
  if (got != (size_t)size) {
    free(raw);
    set_err("db-open: cannot read '%s'", db->path);
    return 0;
  }
  if (memcmp(raw, AINL_DB_MAGIC, 6) != 0) {
    free(raw);
    set_err("db-open: '%s' is not an AINL database", db->path);
    return 0;
  }
  if (raw[6] != AINL_DB_VERSION) {
    unsigned char v = raw[6];
    free(raw);
    set_err("db-open: '%s' is database version %u, but this AINL reads version %d",
            db->path, (unsigned)v, AINL_DB_VERSION);
    return 0;
  }

  size_t off = AINL_DB_HEADER_LEN;
  while (off + AINL_DB_REC_HEADER_LEN <= (size_t)size) {
    uint32_t klen = db_rd32(raw + off);
    uint32_t vlen = db_rd32(raw + off + 4);
    uint32_t want = db_rd32(raw + off + 8);
    size_t body = off + AINL_DB_REC_HEADER_LEN;
    /* The lengths are crash-controlled: bound them by what is actually in the
     * file *before* slicing, so a corrupt 0xFFFFFFFF cannot drive a giant
     * copy. Same rule as the Rust replay. */
    if ((size_t)klen > (size_t)size - body)
      break;
    if ((size_t)vlen > (size_t)size - body - klen)
      break;
    size_t end = body + klen + vlen;
    const unsigned char *kb = raw + body;
    const unsigned char *vb = raw + body + klen;
    if (db_crc(kb, klen, vb, vlen) != want)
      break;
    if (!db_usable(kb, klen) || !db_usable(vb, vlen))
      break;
    char *key = malloc(klen + 1);
    char *val = malloc(vlen + 1);
    if (!key || !val) {
      free(key);
      free(val);
      free(raw);
      set_err("db-open: out of memory");
      return 0;
    }
    memcpy(key, kb, klen);
    key[klen] = 0;
    memcpy(val, vb, vlen);
    val[vlen] = 0;
    db_index_put(db, key, val);
    free(key);
    free(val);
    if (g_err) {
      free(raw);
      return 0;
    }
    off = end;
  }
  free(raw);

  /* Drop the torn tail *on disk*, not just in memory — see the module note in
   * db.rs: a recovery that leaves the garbage in place makes the next open the
   * one that loses data. */
  if (off != (size_t)size) {
    if (ftruncate(fileno(db->f), (off_t)off) != 0) {
      set_err("db-open: cannot truncate '%s'", db->path);
      return 0;
    }
  }
  if (fseek(db->f, 0, SEEK_END) != 0) {
    set_err("db-open: cannot seek '%s'", db->path);
    return 0;
  }
  return 1;
}

static Db *db_lookup(int64_t h, const char *who) {
  if (h < 1 || h > AINL_DB_MAX_OPEN || !g_dbs[h - 1]) {
    set_err("%s: handle %lld is not open", who, (long long)h);
    return NULL;
  }
  return g_dbs[h - 1];
}

/* A handle operand, reported under the builtin's own name. */
/* `as_str_arg`, but naming what was expected — the table layer's messages have
 * to be byte-identical to the interpreter's, and ainl-core/src/dbtab.rs says
 * "expects a str table name" where the bare helper says "expects a str".
 *
 * Added rather than changing `as_str_arg` because the bare form is what every
 * earlier builtin uses, and db_kv_aot.rs documents that those two messages
 * already differ between the engines; making them agree too is a different card.
 * This one is only used by the table layer, which is written to agree exactly. */
static const char *as_str_named(Value *v, const char *who, const char *what) {
  if (v->tag == V_STR)
    return v->u.s->data;
  set_err("%s expects a str %s, got %s", who, what, type_name(v));
  return NULL;
}

static int64_t as_handle_arg(Value *v, const char *who) {
  if (v->tag == V_INT)
    return v->u.i;
  set_err("%s expects a db handle, got %s", who, type_name(v));
  return 0;
}

/* (db-open path) -> int handle. The capacity check runs BEFORE the file is
 * created: opening a database is a filesystem side effect, and a refused open
 * must not leave behind a file the caller never asked for. */
static Value builtin_db_open(Value *args, int nargs) {
  if (nargs != 1) {
    set_err("db-open expects (db-open path)");
    return v_nil();
  }
  const char *path = as_path_arg(&args[0], "db-open");
  if (!path)
    return v_nil();
  int slot = -1;
  for (int i = 0; i < AINL_DB_MAX_OPEN; i++) {
    if (!g_dbs[i]) {
      slot = i;
      break;
    }
  }
  if (slot < 0) {
    set_err("db-open: too many open databases (max %d)", AINL_DB_MAX_OPEN);
    return v_nil();
  }
  /* "r+b" not "w+b": the log being replayed must not be truncated. A missing
   * file is created — "a+b" appends, but seek-to-end before every read is
   * then the caller's problem, so create-then-open is clearer. */
  FILE *f = fopen(path, "r+b");
  if (!f)
    f = fopen(path, "w+b");
  if (!f) {
    set_err("db-open: cannot open '%s'", path);
    return v_nil();
  }
  Db *db = calloc(1, sizeof(Db));
  if (!db) {
    fclose(f);
    set_err("db-open: out of memory");
    return v_nil();
  }
  db->path = strdup(path);
  db->f = f;
  if (!db->path) {
    db_close_raw(db);
    set_err("db-open: out of memory");
    return v_nil();
  }
  if (!db_replay(db)) {
    db_close_raw(db);
    return v_nil();
  }
  g_dbs[slot] = db;
  return v_int(slot + 1);
}

/* Append one record to the log and index it. The single write path for the
 * byte layer, extracted so the value layer appends through exactly the same code
 * — a second writer would be a second place for the two engines to disagree
 * about what a record on disk looks like, and the whole parity claim rests on
 * them writing the same bytes.
 *
 * `who` names the calling builtin in a write failure, so a value-layer write
 * that fails says `db-set: cannot write …` and not `db-put: …`. */
static int db_append(Db *db, const char *key, const char *val, const char *who) {
  size_t klen = strlen(key), vlen = strlen(val);
  unsigned char hdr[AINL_DB_REC_HEADER_LEN];
  db_wr32(hdr, (uint32_t)klen);
  db_wr32(hdr + 4, (uint32_t)vlen);
  db_wr32(hdr + 8, db_crc((const unsigned char *)key, klen, (const unsigned char *)val, vlen));
  if (fwrite(hdr, 1, sizeof(hdr), db->f) != sizeof(hdr) ||
      (klen && fwrite(key, 1, klen, db->f) != klen) ||
      (vlen && fwrite(val, 1, vlen, db->f) != vlen)) {
    set_err("%s: cannot write '%s'", who, db->path);
    return 0;
  }
  db_index_put(db, key, val);
  return 1;
}

/* (db-put handle key value) -> nil. Appends a record to the log. */
static Value builtin_db_put(Value *args, int nargs) {
  if (nargs != 3) {
    set_err("db-put expects (db-put handle key value)");
    return v_nil();
  }
  int64_t h = as_handle_arg(&args[0], "db-put");
  if (!h)
    return v_nil();
  const char *key = as_str_arg(&args[1], "db-put");
  if (!key)
    return v_nil();
  const char *val = as_str_arg(&args[2], "db-put");
  if (!val)
    return v_nil();
  Db *db = db_lookup(h, "db-put");
  if (!db)
    return v_nil();
  db_append(db, key, val, "db-put");
  return v_nil();
}

/* The key operand of the value-layer builtins.
 *
 * Its own helper rather than `as_str_arg` because the *role* has to be in the
 * message: Rust's `as_key` says "expects a str key" and this has to say the same
 * word-for-word, or the two engines disagree on stderr for a program that mixes
 * the layers. The byte layer's `as_str_arg` ("expects a str") is a different
 * function for a different reason and is left alone.
 *
 * Declared up here, with the other `db_*` argument helpers, because
 * `builtin_db_get_raw` below also uses it. */
static const char *dbkv_key_arg(Value *v, const char *who);

/* (db-get-raw handle key) -> str, or nil if absent. The value comes from the
 * in-memory index, so a get never touches the file.
 *
 * The **byte layer's** reader, renamed from `db-get` when the value layer took
 * that name. It returns the stored bytes with no decoding — including a
 * tombstone, since deciding what a tombstone means is the value layer's job.
 * Its error messages carry this name, so a stale handle says
 * "db-get-raw: handle 1 is not open". */
static Value builtin_db_get_raw(Value *args, int nargs) {
  if (nargs != 2) {
    set_err("db-get-raw expects (db-get-raw handle key)");
    return v_nil();
  }
  int64_t h = as_handle_arg(&args[0], "db-get-raw");
  if (!h)
    return v_nil();
  const char *key = dbkv_key_arg(&args[1], "db-get-raw");
  if (!key)
    return v_nil();
  Db *db = db_lookup(h, "db-get-raw");
  if (!db)
    return v_nil();
  const char *val = db_index_get(db, key);
  return val ? v_str(val) : v_nil();
}

/* (db-flush handle) -> nil. fsync, not just fflush: "on disk" for a crash test
 * means the device, not the process's buffer. */
static Value builtin_db_flush(Value *args, int nargs) {
  if (nargs != 1) {
    set_err("db-flush expects (db-flush handle)");
    return v_nil();
  }
  int64_t h = as_handle_arg(&args[0], "db-flush");
  if (!h)
    return v_nil();
  Db *db = db_lookup(h, "db-flush");
  if (!db)
    return v_nil();
  if (fflush(db->f) != 0 || fsync(fileno(db->f)) != 0) {
    set_err("db-flush: cannot flush '%s'", db->path);
    return v_nil();
  }
  return v_nil();
}

/* (db-close handle) -> nil. Flushes, then releases the slot — the slot is
 * freed even if the flush failed, because the error is already reported and
 * holding the slot would leak the only handle a caller could retry with. */
static Value builtin_db_close(Value *args, int nargs) {
  if (nargs != 1) {
    set_err("db-close expects (db-close handle)");
    return v_nil();
  }
  int64_t h = as_handle_arg(&args[0], "db-close");
  if (!h)
    return v_nil();
  Db *db = db_lookup(h, "db-close");
  if (!db)
    return v_nil();
  g_dbs[h - 1] = NULL;
  if (fflush(db->f) != 0 || fsync(fileno(db->f)) != 0)
    set_err("db-close: cannot flush '%s'", db->path);
  db_close_raw(db);
  return v_nil();
}

/* Release every open database at exit. A program that never called db-close
 * has still written a valid log (each put is appended and the OS flushes on
 * close), but leaving the table populated would be a leak in any host that
 * embeds this runtime; the generated main calls this on the success path. */
static void db_close_all(void) {
  for (int i = 0; i < AINL_DB_MAX_OPEN; i++) {
    if (g_dbs[i]) {
      fflush(g_dbs[i]->f);
      db_close_raw(g_dbs[i]);
      g_dbs[i] = NULL;
    }
  }
}

/* ---- Tier 4 key-value layer --------------------------------------------- *
 *
 * A hand-port of ainl-core/src/dbkv.rs, and the rules are the same as for the
 * `db_*` block above: this is an *independent* implementation of the value layer
 * on top of the same on-disk format, not a binding to the Rust one. There is no
 * FFI between them, so "the engine is shared" is true for the interpreter and
 * the VM and is a hand-port here. See ainl-core/src/dbkv.rs for the reasoning
 * behind each rule; the comments here are only about what C forces.
 *
 * The one genuinely free thing is the JSON encoding: builtin_json_serialize and
 * builtin_json_parse are already in this runtime, so a value is stored by
 * handing it to the same writer the `json-serialize` builtin uses. That is why
 * a value refused by `db-set` is refused with the *same message* as
 * `json-serialize` on both engines — not because the strings were copied, but
 * because there is only one writer and both call it.
 */

/* The tombstone that marks a deleted key. Must equal TOMBSTONE in dbkv.rs.
 *
 * It cannot collide with a stored value because a value in the log is always
 * json-serialize's output, and the writer never emits a bare word starting with
 * '~' — strings are quoted, numbers start with a digit or '-', and the literals
 * are true/false/null. So `~` is unreachable from the value layer, which is
 * what lets deletion reuse the byte layer's record instead of needing a second
 * record type. */
#define AINL_DB_TOMBSTONE "~"

/* JSON-encode `v`, returning a V_STR the caller owns, or nil with g_err set.
 *
 * Uses the runtime's own json writer, so a value `db-set` refuses is refused
 * with the *same message* `json-serialize` gives — not because the strings were
 * copied, but because there is one writer and both call it. That is what makes
 * the refusal identical on this port and on the Rust engine for free. */
static Value dbkv_encode(Value *v) {
  Value args[1];
  args[0] = *v;
  v_ref(&args[0]);
  Value out = builtin_json_serialize(args, 1);
  v_unref(&args[0]);
  if (g_err)
    return v_nil();
  if (out.tag != V_STR) {
    set_err("db-set: internal: json-serialize returned a non-string");
    return v_nil();
  }
  return out;
}

/* JSON-decode `text` into a value.
 *
 * A failure leaves g_err **clear** and returns a flag of 0, because the
 * value layer's message is not the parser's: a db-put string is not a parse
 * error, it is the wrong kind of thing to be reading, and the parser's
 * "unexpected character at position 7" would point inside text the caller never
 * wrote. So the caller decides what to say. */
static int dbkv_decode(const char *text, Value *out) {
  Value args[1];
  args[0] = v_str(text);
  v_ref(&args[0]);
  Value parsed = builtin_json_parse(args, 1);
  v_unref(&args[0]);
  if (g_err) {
    g_err = 0;
    g_errmsg[0] = 0;
    return 0;
  }
  *out = parsed;
  return 1;
}

/* The key operand of the value-layer builtins.
 *
 * Its own helper rather than `as_str_arg` because the *role* has to be in the
 * message: Rust's `as_key` says "expects a str key" and this has to say the same
 * word-for-word, or the two engines disagree on stderr for a program that mixes
 * the layers. The byte layer's `as_str_arg` ("expects a str") is a different
 * function for a different reason and is left alone. */
static const char *dbkv_key_arg(Value *v, const char *who) {
  if (v->tag == V_STR)
    return v->u.s->data;
  set_err("%s expects a str key, got %s", who, type_name(v));
  return NULL;
}

/* The stored bytes for `key`, or NULL if the key is absent or deleted.
 *
 * The returned pointer is owned by the index and is valid until the next
 * put/del on the same handle, so a caller must copy anything it keeps.
 *
 * This is the one place the two engines' indexes genuinely differ, and the
 * difference matters here. The Rust index is a HashMap, so a key written twice
 * has ONE entry holding the newest value. This index chains, so the same key
 * written twice has TWO entries, and db_index_get stops at the first — the
 * newest — which is why "last write wins" works here too. Iterating it, though,
 * visits the key once per write. So every enumeration below must **deduplicate**,
 * or an overwritten key is listed and counted twice while the interpreter lists
 * it once. */
static const char *dbkv_lookup(Db *db, const char *key) {
  const char *val = db_index_get(db, key);
  if (!val || strcmp(val, AINL_DB_TOMBSTONE) == 0)
    return NULL;
  return val;
}

/* Whether `key` is live, tested against its **newest** entry only.
 *
 * A tombstone is usually the newest entry for a deleted key, so this is true
 * exactly when the key has no tombstone as its latest write. An older value
 * still sitting further down the chain is history, not the current answer. */
static int dbkv_is_live(Db *db, const char *key) {
  return dbkv_lookup(db, key) != NULL;
}

/* (db-set handle key value) -> nil. The value is JSON-encoded and handed to
 * the byte layer, so the on-disk format is unchanged from Tier 4 card 1. */
static Value builtin_db_set(Value *args, int nargs) {
  if (nargs != 3) {
    set_err("db-set expects (db-set handle key value)");
    return v_nil();
  }
  int64_t h = as_handle_arg(&args[0], "db-set");
  if (!h)
    return v_nil();
  const char *key = dbkv_key_arg(&args[1], "db-set");
  if (!key)
    return v_nil();
  Db *db = db_lookup(h, "db-set");
  if (!db)
    return v_nil();
  Value encoded = dbkv_encode(&args[2]);
  if (g_err)
    return v_nil();
  db_append(db, key, encoded.u.s->data, "db-set");
  v_unref(&encoded);
  return v_nil();
}

/* (db-get handle key) -> the value, or nil if absent or deleted.
 *
 * The value-level read, replacing the byte layer's binding for this name. See
 * dbkv.rs for the collision and why this side wins. */
static Value builtin_db_get_kv(Value *args, int nargs) {
  if (nargs != 2) {
    set_err("db-get expects (db-get handle key)");
    return v_nil();
  }
  int64_t h = as_handle_arg(&args[0], "db-get");
  if (!h)
    return v_nil();
  const char *key = dbkv_key_arg(&args[1], "db-get");
  if (!key)
    return v_nil();
  Db *db = db_lookup(h, "db-get");
  if (!db)
    return v_nil();
  const char *val = dbkv_lookup(db, key);
  if (!val)
    return v_nil();
  /* Copied before decoding: the decode can reallocate nothing, but the error
   * path below formats `val` and the index owns it, so a plain pointer is fine
   * for both uses and copying would be the only way to be sure. */
  Value decoded;
  if (!dbkv_decode(val, &decoded)) {
    set_err("db-get: '%s' holds text that is not an AINL value (%s); store it with "
            "db-set rather than db-put",
            key, val);
    return v_nil();
  }
  return decoded;
}

/* (db-del handle key) -> true if the key was live, false if it was not.
 *
 * A tombstone is appended even when the key is already absent, so the log
 * records the call. Deletion is a log record for the same reason a write is:
 * it has to survive the process, and replay is the only mechanism that already
 * knows how to do that. */
static Value builtin_db_del(Value *args, int nargs) {
  if (nargs != 2) {
    set_err("db-del expects (db-del handle key)");
    return v_nil();
  }
  int64_t h = as_handle_arg(&args[0], "db-del");
  if (!h)
    return v_nil();
  const char *key = dbkv_key_arg(&args[1], "db-del");
  if (!key)
    return v_nil();
  Db *db = db_lookup(h, "db-del");
  if (!db)
    return v_nil();
  int existed = dbkv_lookup(db, key) != NULL;
  /* Through the same append as the byte layer, so a tombstone is a record the
   * two engines write identically. */
  db_append(db, key, AINL_DB_TOMBSTONE, "db-del");
  return existed ? v_bool(1) : v_bool(0);
}

/* Byte-value order, the same rule list-dir's qsort uses and for the same
 * reason: db-keys must print the same sequence on every backend, and the two
 * engines index keys differently with no defined order of their own. */
static int dbkv_key_cmp(const void *a, const void *b) {
  const char *const *x = a;
  const char *const *y = b;
  return strcmp(*x, *y);
}

/* (db-keys handle) -> every live key, sorted.
 *
 * The index holds every record ever written, so two filters apply here and both
 * are load-bearing:
 *
 *  1. **deduplicate.** Chaining means an overwritten key occupies several
 *     entries, and the newest is first in its chain — so an entry whose key
 *     appears *later* in the same chain is an older write and must be skipped.
 *     Without this an overwritten key is listed and counted twice, which is
 *     exactly the drift the Rust tests caught in the first version of this port.
 *  2. **drop tombstones.** `dbkv_lookup` tests the newest entry, so this is
 *     true only when the key's latest write is not a delete.
 */
static Value builtin_db_keys(Value *args, int nargs) {
  if (nargs != 1) {
    set_err("db-keys expects (db-keys handle)");
    return v_nil();
  }
  int64_t h = as_handle_arg(&args[0], "db-keys");
  if (!h)
    return v_nil();
  Db *db = db_lookup(h, "db-keys");
  if (!db)
    return v_nil();
  size_t n = 0;
  for (int b = 0; b < DB_BUCKETS; b++)
    for (DbEntry *e = db->buckets[b]; e; e = e->next)
      n++;
  char **keys = n ? malloc(n * sizeof(char *)) : NULL;
  Value *items = n ? malloc(n * sizeof(Value)) : NULL;
  if (n && (!keys || !items)) {
    free(keys);
    free(items);
    set_err("db-keys: out of memory");
    return v_nil();
  }
  size_t i = 0;
  for (int b = 0; b < DB_BUCKETS; b++) {
    for (DbEntry *e = db->buckets[b]; e; e = e->next) {
      /* Newest write for this key? The first match in a chain is the newest, so
       * a later match is an older record of the same key. */
      if (db_index_get(db, e->key) != e->val)
        continue;
      if (!dbkv_is_live(db, e->key))
        continue;
      keys[i++] = e->key;
    }
  }
  n = i;
  if (n > 1)
    qsort(keys, n, sizeof(char *), dbkv_key_cmp);
  for (size_t k = 0; k < n; k++)
    items[k] = v_str(keys[k]);
  free(keys);
  return v_list_from_array(items, (int)n);
}

/* (db-count handle) -> how many keys are live.
 *
 * The same two filters as `db-keys`, and for the same two reasons. Counting
 * records instead of keys would grow without bound as keys were overwritten —
 * the same growth the log has — and would answer a question nobody asked. */
static Value builtin_db_count(Value *args, int nargs) {
  if (nargs != 1) {
    set_err("db-count expects (db-count handle)");
    return v_nil();
  }
  int64_t h = as_handle_arg(&args[0], "db-count");
  if (!h)
    return v_nil();
  Db *db = db_lookup(h, "db-count");
  if (!db)
    return v_nil();
  int n = 0;
  for (int b = 0; b < DB_BUCKETS; b++) {
    for (DbEntry *e = db->buckets[b]; e; e = e->next) {
      if (db_index_get(db, e->key) != e->val)
        continue;
      if (dbkv_is_live(db, e->key))
        n++;
    }
  }
  return v_int(n);
}

/* ---- Tier 4 table layer ------------------------------------------------- *
 *
 * A hand-port of ainl-core/src/dbtab.rs, and the rules are the same as for the
 * `db_*` and `dbkv` blocks above: this is an *independent* implementation on top
 * of the same on-disk format, not a binding to the Rust one. There is no FFI
 * anywhere in this project, so "the engine is shared" is true for the
 * interpreter and the VM and is a second implementation here. Read this block
 * side by side with dbtab.rs; the comments here are only about what C forces.
 *
 * ## What is hand-ported, and what is not
 *
 * The B-tree (`dbt_*`, below) is ported in full, because it cannot be borrowed:
 * there is no FFI, and the alternative — a flat scan — would make the compiled
 * binary's lookup cost different in *kind* from the interpreter's, which is the
 * one thing this card exists to prevent. So insert, lookup, delete and the
 * in-order walk are all here, in that order, with the same minimum-fill repair
 * and the same median rule.
 *
 * What is *not* re-derived is the row encoding: the length-prefixed key
 * (`@t:<len>:<name><key-json>`), the JSON text of a row, and the tombstone all
 * come from the same three places the other layers use — `db_append` writes the
 * record, `builtin_json_serialize` produces the row text, and
 * `AINL_DB_TOMBSTONE` is the delete marker. Sharing those is deliberate: a
 * format invented here and a format invented in Rust would be two formats, and
 * the whole point of this layer is that one file works on both engines.
 *
 * ## The one place C is genuinely harder: ownership
 *
 * The Rust tree is `Vec<String>` and drops it for free. Here every key and value
 * is a malloc'd `char *` and every node is malloc'd too, so each of the split,
 * merge and borrow paths below has to say what it hands over and what it takes.
 * The rule used throughout: **a function that takes a node out of its parent
 * owns it**, and every node that is not owned by the tree is freed by exactly
 * one path. `dbt_node_free` is the single place that walks a node, so a leak or
 * a double free is a bug in one function rather than spread across the port.
 *
 * ## The invariant the port must not break
 *
 * Every non-root node holds between DBT_MIN_KEYS and DBT_MAX_KEYS keys, and all
 * leaves are at one depth. `crates/ainl-cc/tests/dbtab_aot.rs` runs the *same*
 * operation sequence as the Rust unit tests and asserts the same results, which
 * is what makes "the port is a port" a checked claim rather than a reviewer's
 * opinion. It is the strongest test in the file: the two implementations share
 * no code, so agreement is evidence.
 */

#define DBT_MAX_KEYS 15
#define DBT_MIN_KEYS 7

/* Marks a log key as a table row. Must equal ROW_PREFIX in dbtab.rs. */
#define DBT_ROW_PREFIX "@t:"

/* The empty primary key, which no user row can have because a row's key is
 * always at least two bytes of JSON. Must equal TABLE_MARKER_KEY. */
#define DBT_MARKER_KEY ""

typedef struct DbtNode DbtNode;
struct DbtNode {
  char **keys;
  char **vals;
  DbtNode **kids;
  int nkeys;
  int nkids;
};

/* A whole table: the tree, plus a count so `len` is O(1). */
typedef struct DbtTree DbtTree;
struct DbtTree {
  DbtNode *root;
  size_t count;
};

static void dbt_node_free(DbtNode *n) {
  if (!n)
    return;
  for (int i = 0; i < n->nkeys; i++) {
    free(n->keys[i]);
    free(n->vals[i]);
  }
  for (int i = 0; i < n->nkids; i++)
    dbt_node_free(n->kids[i]);
  free(n->keys);
  free(n->vals);
  free(n->kids);
  free(n);
}

static DbtNode *dbt_node_new(void) {
  DbtNode *n = calloc(1, sizeof(DbtNode));
  if (!n)
    set_err("db: out of memory");
  return n;
}

/* Append one key (taking ownership of `k`) and its value to a node.
 *
 * `k` and `v` are both owned by the node once this returns, including on
 * failure — so every caller can free its own reference immediately and there is
 * no path where a key leaks because a later allocation failed. */
static void dbt_node_push(DbtNode *n, char *k, char *v) {
  char **nk = realloc(n->keys, sizeof(char *) * (size_t)(n->nkeys + 1));
  char **nv = realloc(n->vals, sizeof(char *) * (size_t)(n->nkeys + 1));
  DbtNode **nc = realloc(n->kids, sizeof(DbtNode *) * (size_t)(n->nkids + 1));
  if (!nk || !nv || !nc) {
    /* Out of memory mid-insert. The caller unwinds by dropping the whole tree,
     * and the keys pushed so far are already reachable from `n`, so freeing
     * this node is enough — which is why they are stored before the failure is
     * reported. */
    set_err("db: out of memory");
    return;
  }
  n->keys = nk;
  n->vals = nv;
  n->kids = nc;
  n->keys[n->nkeys] = k;
  n->vals[n->nkeys] = v;
  n->nkeys++;
  n->nkids++;
}

/* Insert a child at index `i`, taking ownership of `kid`. Keeps the invariant
 * that there is one more child than key once the key is added. */
static void dbt_node_insert_kid(DbtNode *n, int i, DbtNode *kid) {
  DbtNode **nc = realloc(n->kids, sizeof(DbtNode *) * (size_t)(n->nkids + 1));
  if (!nc) {
    set_err("db: out of memory");
    return;
  }
  n->kids = nc;
  for (int k = n->nkids; k > i; k--)
    n->kids[k] = n->kids[k - 1];
  n->kids[i] = kid;
  n->nkids++;
}

static int dbt_is_leaf(const DbtNode *n) { return n->nkids == 0; }

/* Where `key` would go: the number of keys strictly smaller than it, by
 * memcmp. Must match `Node::search` in btree.rs — this comparator is the whole
 * reason `db-all-rows` agrees across engines. */
static int dbt_search(const DbtNode *n, const char *key) {
  int lo = 0, hi = n->nkeys;
  while (lo < hi) {
    int mid = lo + (hi - lo) / 2;
    size_t la = strlen(n->keys[mid]), lb = strlen(key);
    int c = memcmp(n->keys[mid], key, la < lb ? la : lb);
    if (c == 0)
      c = la < lb ? -1 : (la > lb ? 1 : 0);
    if (c < 0)
      lo = mid + 1;
    else
      hi = mid;
  }
  return lo;
}

/* Split an overfull node. The median goes back through `*out_key`/`*out_val`
 * (both owned by the caller) and the new sibling is returned. A leaf hands over
 * no children, which is the case that makes this different from an interior
 * node and the one the first version of this port got wrong. */
static DbtNode *dbt_split(DbtNode *n, char **out_key, char **out_val) {
  int mid = n->nkeys / 2;
  *out_key = n->keys[mid];
  *out_val = n->vals[mid];
  DbtNode *right = dbt_node_new();
  if (!right)
    return NULL;
  /* Right half: keys mid+1..end, and children mid+1..end (or none for a leaf). */
  int rkeys = n->nkeys - (mid + 1);
  right->keys = malloc(sizeof(char *) * (size_t)(rkeys > 0 ? rkeys : 1));
  right->vals = malloc(sizeof(char *) * (size_t)(rkeys > 0 ? rkeys : 1));
  if (!right->keys || !right->vals) {
    set_err("db: out of memory");
    return NULL;
  }
  for (int i = 0; i < rkeys; i++) {
    right->keys[i] = n->keys[mid + 1 + i];
    right->vals[i] = n->vals[mid + 1 + i];
  }
  right->nkeys = rkeys;
  int rkids = dbt_is_leaf(n) ? 0 : n->nkids - (mid + 1);
  if (rkids > 0) {
    right->kids = malloc(sizeof(DbtNode *) * (size_t)rkids);
    if (!right->kids) {
      set_err("db: out of memory");
      return NULL;
    }
    for (int i = 0; i < rkids; i++)
      right->kids[i] = n->kids[mid + 1 + i];
  }
  right->nkids = rkids;
  /* Left half keeps keys 0..mid-1 and children 0..mid (mid+1 of them). */
  n->nkeys = mid;
  n->nkids = dbt_is_leaf(n) ? 0 : mid + 1;
  return right;
}

/* Remove index `i` from a node, freeing the key and the value. */
static void dbt_remove_at(DbtNode *n, int i) {
  free(n->keys[i]);
  free(n->vals[i]);
  for (int s = i; s < n->nkeys - 1; s++) {
    n->keys[s] = n->keys[s + 1];
    n->vals[s] = n->vals[s + 1];
  }
  n->nkeys--;
}

/* Insert at index `i`, taking ownership of both strings.
 *
 * The realloc comes *before* the shift, always. Growing after the shift writes
 * one element past the end of a full array, and the two places that do this
 * (here, and the leaf path of `dbt_insert_into`) have to agree — or the bug
 * comes back in whichever copy someone edits. */
static void dbt_insert_at(DbtNode *n, int i, char *k, char *v) {
  char **nk = realloc(n->keys, sizeof(char *) * (size_t)(n->nkeys + 1));
  char **nv = realloc(n->vals, sizeof(char *) * (size_t)(n->nkeys + 1));
  if (!nk || !nv) {
    set_err("db: out of memory");
    return;
  }
  n->keys = nk;
  n->vals = nv;
  for (int s = n->nkeys; s > i; s--) {
    n->keys[s] = n->keys[s - 1];
    n->vals[s] = n->vals[s - 1];
  }
  n->keys[i] = k;
  n->vals[i] = v;
  n->nkeys++;
}

/* Remove and return the child at `i`; the caller owns it. */
static DbtNode *dbt_node_take_kid(DbtNode *n, int i) {
  DbtNode *kid = n->kids[i];
  for (int k = i; k < n->nkids - 1; k++)
    n->kids[k] = n->kids[k + 1];
  n->nkids--;
  return kid;
}

/* Insert into the subtree at `n`, promoting through `parent` if it overflows.
 * `*up_key`/`*up_val` and the returned sibling are owned by the caller.
 *
 * Repairing on the way *up* after a delete is what makes the delete correct; the
 * insert is symmetric and simpler, and the shared shape is deliberate. */
static DbtNode *dbt_insert_into(Db *db, DbtNode *n, const char *key, const char *val,
                                char **up_key, char **up_val) {
  int i = dbt_search(n, key);
  if (i < n->nkeys && strcmp(n->keys[i], key) == 0) {
    /* Already present: replace. A tree that kept both copies would have two
     * answers for one key and a lookup would return only one of them. */
    free(n->vals[i]);
    n->vals[i] = strdup(val);
    return NULL;
  }
  if (dbt_is_leaf(n)) {
    /* Insert at `i`, taking copies so the caller's strings are untouched.
     *
     * `dbt_insert_at` reallocates *before* it shifts. That order is not
     * incidental: the first version of this port shifted first and reallocated
     * after, which wrote one element past the end of a full array — a heap
     * overflow that stayed silent on a 3-key table and crashed on the next
     * insert into a full leaf. `dbt_insert_at` exists so that order is written
     * once, here, and the interior path below uses it too. */
    char *k = strdup(key), *v = strdup(val);
    if (!k || !v) {
      free(k);
      free(v);
      set_err("db: out of memory");
      return NULL;
    }
    dbt_insert_at(n, i, k, v);
    if (g_err)
      return NULL;
  } else {
    char *ck = NULL, *cv = NULL;
    DbtNode *right = dbt_insert_into(db, n->kids[i], key, val, &ck, &cv);
    if (!right)
      return NULL;
    /* The promoted separator belongs to the child, which is about to become
     * someone else's child, so this node takes its own copy. */
    char *k2 = strdup(ck), *v2 = strdup(cv);
    if (!k2 || !v2) {
      free(k2);
      free(v2);
      set_err("db: out of memory");
      return NULL;
    }
    /* The child grew a sibling, so this node gains a key and a child. Same
     * helper as the leaf path, and for the same reason: it reallocates before
     * shifting. The interior path had its own copy of that loop, shifting first,
     * which corrupted the root's separator array on the 17th insert — the first
     * one to descend past a split root — and made a row that was in the tree
     * unfindable. Two copies of this loop is exactly the kind of duplication
     * that lets one copy be wrong; there is now one. */
    dbt_insert_at(n, i, k2, v2);
    if (g_err)
      return NULL;
    dbt_node_insert_kid(n, i + 1, right);
  }
  if (n->nkeys > DBT_MAX_KEYS) {
    char *pk = NULL, *pv = NULL;
    DbtNode *right = dbt_split(n, &pk, &pv);
    if (!right)
      return NULL;
    *up_key = pk;
    *up_val = pv;
    return right;
  }
  return NULL;
}

/* The value for `key`, or NULL. The stored pointer belongs to the tree and is
 * valid until the next insert or remove on it. */
static const char *dbt_get(const DbtNode *n, const char *key) {
  /* A `while`, not a `for`: this is the descent. The first version used
   * `for (cur = n; cur; cur = NULL)` and moved the cursor inside the body, which
   * made the loop test `NULL` *after* the first hop and so stop at depth one.
   * Every table with 15 keys or fewer is a single leaf, and every table of 16 to
   * 31 is two levels, so it passed every small test and failed on the first
   * lookup that had to go past the root — a `db-delete-row` of an existing row
   * returning false because the key was one level down. */
  for (const DbtNode *cur = n; cur;) {
    int i = dbt_search(cur, key);
    if (i < cur->nkeys && strcmp(cur->keys[i], key) == 0)
      return cur->vals[i];
    if (dbt_is_leaf(cur))
      return NULL;
    cur = cur->kids[i];
  }
  return NULL;
}


/* The largest key in a subtree: the in-order predecessor of the next key. */
static char *dbt_rightmost_key(DbtNode *n, char **out_val) {
  DbtNode *cur = n;
  while (!dbt_is_leaf(cur))
    cur = cur->kids[cur->nkids - 1];
  *out_val = strdup(cur->vals[cur->nkeys - 1]);
  return strdup(cur->keys[cur->nkeys - 1]);
}

/* Borrow the separator down into the underfull child at `ci` and a sibling key
 * up into this node, from the left sibling. A leaf sibling lends no child. */
static void dbt_borrow_from_left(DbtNode *n, int ci) {
  char *sep = n->keys[ci - 1];
  char *sep_val = n->vals[ci - 1];
  DbtNode *left = n->kids[ci - 1];
  char *up = left->keys[left->nkeys - 1];
  char *up_val = left->vals[left->nkeys - 1];
  left->nkeys--;
  DbtNode *moved = NULL;
  if (!dbt_is_leaf(left))
    moved = dbt_node_take_kid(left, left->nkids - 1);
  n->keys[ci - 1] = up;
  n->vals[ci - 1] = up_val;
  DbtNode *child = n->kids[ci];
  dbt_insert_at(child, 0, sep, sep_val);
  if (moved)
    dbt_node_insert_kid(child, 0, moved);
}

/* The mirror: the separator `keys[ci]` descends, the right sibling's smallest
 * key rises. */
static void dbt_borrow_from_right(DbtNode *n, int ci) {
  char *sep = n->keys[ci];
  char *sep_val = n->vals[ci];
  DbtNode *right = n->kids[ci + 1];
  char *up = right->keys[0];
  char *up_val = right->vals[0];
  dbt_remove_at(right, 0);
  DbtNode *moved = NULL;
  if (!dbt_is_leaf(right))
    moved = dbt_node_take_kid(right, 0);
  n->keys[ci] = up;
  n->vals[ci] = up_val;
  DbtNode *child = n->kids[ci];
  dbt_node_push(child, sep, sep_val);
  if (moved)
    dbt_node_insert_kid(child, child->nkids - 1, moved);
}

/* Merge the child at `left_ci + 1` into the one at `left_ci`. The right child
 * is consumed. */
static void dbt_merge(DbtNode *n, int left_ci) {
  char *sep = n->keys[left_ci];
  char *sep_val = n->vals[left_ci];
  DbtNode *right = dbt_node_take_kid(n, left_ci + 1);
  dbt_remove_at(n, left_ci);
  DbtNode *left = n->kids[left_ci];
  dbt_node_push(left, sep, sep_val);
  for (int i = 0; i < right->nkeys; i++) {
    char *k = right->keys[i], *v = right->vals[i];
    right->keys[i] = NULL;
    right->vals[i] = NULL;
    dbt_node_push(left, k, v);
  }
  for (int i = 0; i < right->nkids; i++) {
    DbtNode *kid = right->kids[i];
    right->kids[i] = NULL;
    dbt_node_insert_kid(left, left->nkids - 1, kid);
  }
  free(right->keys);
  free(right->vals);
  free(right->kids);
  free(right);
}

/* Refill the underfull child at `ci`. Returns the index of the child that now
 * holds what the underfull one held. */
static int dbt_fix_child(DbtNode *n, int ci) {
  if (n->nkids < 2)
    return ci; /* the root's single child: the root is exempt from the minimum */
  if (ci > 0 && n->kids[ci - 1]->nkeys > DBT_MIN_KEYS) {
    dbt_borrow_from_left(n, ci);
    return ci;
  }
  if (ci + 1 < n->nkids && n->kids[ci + 1]->nkeys > DBT_MIN_KEYS) {
    dbt_borrow_from_right(n, ci);
    return ci;
  }
  if (ci > 0) {
    dbt_merge(n, ci - 1);
    return ci - 1;
  }
  dbt_merge(n, 0);
  return 0;
}

/* Delete `key` from the subtree at `n`, repairing on the way back up. The node
 * is known to contain the key. */
static void dbt_delete_from(DbtNode *n, const char *key) {
  int i = dbt_search(n, key);
  if (dbt_is_leaf(n)) {
    dbt_remove_at(n, i);
    return;
  }
  if (i < n->nkeys && strcmp(n->keys[i], key) == 0) {
    /* An interior key: replace it with its in-order predecessor, then delete
     * that from the left child, which is a leaf-side delete and so simple. */
    char *pred_val = NULL;
    char *pred = dbt_rightmost_key(n->kids[i], &pred_val);
    free(n->keys[i]);
    free(n->vals[i]);
    n->keys[i] = pred;
    n->vals[i] = pred_val;
    dbt_delete_from(n->kids[i], pred);
  } else {
    dbt_delete_from(n->kids[i], key);
  }
  if (n->kids[i]->nkeys < DBT_MIN_KEYS)
    dbt_fix_child(n, i);
}

static DbtTree *dbt_new(void) {
  DbtTree *t = calloc(1, sizeof(DbtTree));
  if (!t) {
    set_err("db: out of memory");
    return NULL;
  }
  t->root = dbt_node_new();
  if (!t->root) {
    free(t);
    return NULL;
  }
  t->count = 0;
  return t;
}

static void dbt_free(DbtTree *t) {
  if (!t)
    return;
  dbt_node_free(t->root);
  free(t);
}

/* Insert, or replace if the key is there. Returns 1 if the key was new. */
static int dbt_insert(DbtTree *t, const char *key, const char *val) {
  if (dbt_get(t->root, key)) {
    /* Replace in place, which cannot overflow a node. */
    int i = dbt_search(t->root, key);
    DbtNode *cur = t->root;
    while (1) {
      i = dbt_search(cur, key);
      if (i < cur->nkeys && strcmp(cur->keys[i], key) == 0) {
        free(cur->vals[i]);
        cur->vals[i] = strdup(val);
        return 0;
      }
      if (dbt_is_leaf(cur))
        break;
      cur = cur->kids[i];
    }
    return 0;
  }
  char *up_key = NULL, *up_val = NULL;
  DbtNode *right = dbt_insert_into(NULL, t->root, key, val, &up_key, &up_val);
  if (!right)
    return 1;
  DbtNode *new_root = dbt_node_new();
  if (!new_root)
    return 1;
  new_root->keys = malloc(sizeof(char *));
  new_root->vals = malloc(sizeof(char *));
  new_root->kids = malloc(sizeof(DbtNode *) * 2);
  if (!new_root->keys || !new_root->vals || !new_root->kids) {
    set_err("db: out of memory");
    return 1;
  }
  new_root->keys[0] = up_key;
  new_root->vals[0] = up_val;
  new_root->nkeys = 1;
  DbtNode *old = t->root;
  new_root->kids[0] = old;
  new_root->kids[1] = right;
  new_root->nkids = 2;
  t->root = new_root;
  t->count++;
  return 1;
}

/* Remove `key`. Returns 1 if it was there. */
static int dbt_remove(DbtTree *t, const char *key) {
  if (!dbt_get(t->root, key))
    return 0;
  dbt_delete_from(t->root, key);
  /* The root lost its only key: the child becomes the new root, so the tree
   * loses a level instead of keeping an underfull one. */
  if (t->root->nkeys == 0 && t->root->nkids == 1) {
    DbtNode *child = dbt_node_take_kid(t->root, 0);
    dbt_node_free(t->root);
    t->root = child;
  }
  t->count--;
  return 1;
}

/* How many levels; a single leaf is 1. */
static int dbt_height(const DbtTree *t) {
  int h = 1;
  for (const DbtNode *n = t->root; n && !dbt_is_leaf(n); n = n->kids[0])
    h++;
  return h;
}

/* Walk the tree in key order, calling `fn` per pair. The callback returns 0 to
 * stop. Used by `db-all-rows` and by the tests. */
static void dbt_walk(const DbtNode *n,
                     void (*fn)(const char *, const char *, void *), void *ctx) {
  for (int i = 0; i < n->nkeys; i++) {
    if (i < n->nkids)
      dbt_walk(n->kids[i], fn, ctx);
    fn(n->keys[i], n->vals[i], ctx);
  }
  if (n->nkids > n->nkeys)
    dbt_walk(n->kids[n->nkids - 1], fn, ctx);
}

/* ---- the log key encoding ----------------------------------------------- */

/* "@t:<len>:<name><key-json>" — length-prefixed so no table name or key can
 * make two pairs encode alike. Must equal `row_key` in dbtab.rs, including the
 * length being the name's **byte** length. */
static char *dbt_row_key(const char *table, const char *key_json) {
  size_t tl = strlen(table), kl = strlen(key_json);
  char *out = malloc(tl + kl + 32);
  if (!out) {
    set_err("db: out of memory");
    return NULL;
  }
  snprintf(out, tl + kl + 32, "%s%zu:%s%s", DBT_ROW_PREFIX, tl, table, key_json);
  return out;
}

/* The table a log key belongs to, or NULL if it is not a row key.
 *
 * The caller owns the returned string. Only the table name is needed: the
 * primary key is the rest, and every operation re-derives it from the value it
 * is given rather than reading it back out of the log key. */
static char *dbt_row_table(const char *logkey) {
  size_t pl = strlen(DBT_ROW_PREFIX);
  if (strncmp(logkey, DBT_ROW_PREFIX, pl) != 0)
    return NULL;
  const char *rest = logkey + pl;
  const char *colon = strchr(rest, ':');
  if (!colon || colon == rest)
    return NULL;
  /* Parse the length, then copy exactly that many bytes. A length that runs
   * past the end, or that would cut a multi-byte character, yields NULL rather
   * than a truncated name — the same refusal dbtab.rs's `get(..len)` gives. */
  char *endp = NULL;
  unsigned long len = strtoul(rest, &endp, 10);
  if (!endp || *endp != ':')
    return NULL;
  const char *name = colon + 1;
  size_t avail = strlen(name);
  if (len > avail)
    return NULL;
  char *out = malloc(len + 1);
  if (!out) {
    set_err("db: out of memory");
    return NULL;
  }
  memcpy(out, name, len);
  out[len] = 0;
  return out;
}

/* ---- per-database table sets -------------------------------------------- */

/* One table, by name. A fixed-size slot array rather than a linked list, for
 * the same reason the handle table is: a linked list would need a comparison
 * per lookup, and the number of tables a program creates is small and known at
 * the point it creates them. 64 is a guess, and a generous one — but a fixed
 * table means a bounded, predictable memory cost per open database, which is
 * the property that keeps the "no allocator accounting" promise. */
#define DBT_MAX_TABLES 64

typedef struct DbtTable DbtTable;
struct DbtTable {
  char *name;
  DbtTree *tree;
};

/* The table set for one open database, held on the Db itself so it has exactly
 * the same lifetime as the handle. Built lazily, like the Rust one: a program
 * that only ever calls `db-set` never allocates a tree. */
typedef struct DbtSet DbtSet;
struct DbtSet {
  DbtTable *tabs[DBT_MAX_TABLES];
  int n;
};

static void dbt_set_free(DbtSet *s) {
  for (int i = 0; i < s->n; i++) {
    free(s->tabs[i]->name);
    dbt_free(s->tabs[i]->tree);
    free(s->tabs[i]);
  }
  s->n = 0;
}

static DbtTable *dbt_set_find(DbtSet *s, const char *name) {
  for (int i = 0; i < s->n; i++)
    if (strcmp(s->tabs[i]->name, name) == 0)
      return s->tabs[i];
  return NULL;
}

/* Create `name`, or return it if it exists. Idempotent — see dbtab.rs. */
static DbtTable *dbt_set_create(DbtSet *s, const char *name) {
  DbtTable *t = dbt_set_find(s, name);
  if (t)
    return t;
  if (s->n >= DBT_MAX_TABLES) {
    set_err("db-create-table: too many tables in one database (max %d)",
            DBT_MAX_TABLES);
    return NULL;
  }
  t = calloc(1, sizeof(DbtTable));
  if (!t) {
    set_err("db: out of memory");
    return NULL;
  }
  t->name = strdup(name);
  t->tree = dbt_new();
  if (!t->name || !t->tree) {
    free(t->name);
    dbt_free(t->tree);
    free(t);
    return NULL;
  }
  s->tabs[s->n++] = t;
  return t;
}

/* Build the set from a replayed log. Called once per open, on the same records
 * the hash index already holds, so a torn tail is already gone. */
static DbtSet *dbt_set_rebuild(Db *db) {
  DbtSet *s = calloc(1, sizeof(DbtSet));
  if (!s) {
    set_err("db: out of memory");
    return NULL;
  }
  /* The index chain holds *every* record for a key, not just the newest — the
   * KV layer relies on that, because `db-get` walks the chain and takes the
   * first match. So the chain is in file order, newest first, and a table row
   * that was inserted and then deleted appears twice: the live row, then the
   * tombstone. Replaying both as plain inserts resurrects the deleted row,
   * which is what the first version of this function did.
   *
   * The fix is "first sighting wins": for each row key, apply only the newest
   * record and ignore the rest. That is exactly the rule `db-get` applies, and
   * applying it here is what makes a table's rebuild agree with the byte
   * layer's own idea of a key's current value.
   *
   * `seen` is an open-addressed set keyed by `db_hash`, sized from the record
   * count. It has to be a hash set and not a list: a linear scan would make
   * reopening an n-row table O(n^2) in string comparisons, which is the whole
   * cost the B-tree exists to avoid paying. The table holds borrowed pointers
   * into the index, so it allocates once and frees once. */
  size_t cap = 16;
  while (cap < (size_t)db->nkeys * 2 + 2)
    cap *= 2;
  char **seen = calloc(cap, sizeof(char *));
  if (!seen) {
    set_err("db: out of memory");
    return s;
  }
  size_t seen_count = 0;
  int overflowed = 0;

  for (int b = 0; b < DB_BUCKETS; b++) {
    for (DbEntry *e = db->buckets[b]; e; e = e->next) {
      char *table = dbt_row_table(e->key);
      if (!table)
        continue;
      DbtTable *t = dbt_set_create(s, table);
      if (!t) {
        free(table);
        break;
      }
      /* The key is the part of the log key after the table name, which
       * `dbt_row_table` already skipped past; recompute it rather than trust a
       * second parse to agree. */
      char *rk = dbt_row_key(table, "");
      if (!rk) {
        free(table);
        break;
      }
      const char *keyjson = e->key + strlen(rk);
      free(rk);

      /* Has this row key already been applied? Open addressing with linear
       * probing; a NULL slot is empty, so the table never has to be cleared. */
      size_t slot = db_hash(e->key) & (cap - 1);
      int dup = 0;
      while (seen[slot]) {
        if (strcmp(seen[slot], e->key) == 0) {
          dup = 1;
          break;
        }
        slot = (slot + 1) & (cap - 1);
      }
      if (dup) {
        free(table);
        continue;
      }
      /* The table is sized at 2x the record count, so it cannot fill. Guarded
       * anyway: if it ever did, the worst case is a wrong answer, so it is
       * reported rather than allowed. */
      if (seen_count * 2 >= cap) {
        overflowed = 1;
        free(table);
        break;
      }
      seen[slot] = e->key;
      seen_count++;

      if (strcmp(keyjson, DBT_MARKER_KEY) != 0) {
        if (strcmp(e->val, AINL_DB_TOMBSTONE) == 0)
          dbt_remove(t->tree, keyjson);
        else
          dbt_insert(t->tree, keyjson, e->val);
      }
      free(table);
    }
  }
  free(seen);
  if (overflowed)
    set_err("db: too many records to index in one database");
  return s;
}

/* The table set for this handle, building it on first use. */
static DbtSet *db_tables(Db *db) {
  if (!db->tables)
    db->tables = dbt_set_rebuild(db);
  return db->tables;
}

/* ---- the builtins -------------------------------------------------------- */

/* The key text of a primary key, or NULL with g_err set. Mirrors `key_json` in
 * dbtab.rs: only a scalar is a legal key, and the JSON writer is what both
 * engines use, so the bytes agree by construction rather than by convention. */
static char *dbt_key_json(Value *v, const char *who) {
  if (v->tag == V_LIST || v->tag == V_MAP) {
    set_err("%s: primary key cannot be a %s — a table is indexed on one column, "
            "and a composite or unordered key has no order to index by",
            who, type_name(v));
    return NULL;
  }
  if (v->tag == V_CLOSURE || v->tag == V_BUILTIN || v->tag == V_SYM) {
    set_err("%s: primary key cannot be a %s", who, type_name(v));
    return NULL;
  }
  Value enc = dbkv_encode(v);
  if (g_err)
    return NULL;
  char *out = strdup(enc.u.s->data);
  v_unref(&enc);
  if (!out)
    set_err("db: out of memory");
  return out;
}

/* The row operand: a non-empty list. */
static int dbt_row_of(Value *v, Value **out, int *n, const char *who) {
  if (v->tag != V_LIST) {
    set_err("%s expects a list row, got %s", who, type_name(v));
    return 0;
  }
  ConsCell *c = v->u.l;
  *n = (int)c->len;
  if (*n == 0) {
    set_err("%s: a row needs a primary key, so it cannot be empty", who);
    return 0;
  }
  Value *items = malloc(sizeof(Value) * (size_t)*n);
  if (!items) {
    set_err("db: out of memory");
    return 0;
  }
  int k = 0;
  for (; c; c = c->tail)
    items[k++] = c->head;
  *out = items;
  return 1;
}

/* (db-create-table handle name) -> the name. Idempotent. */
static Value builtin_db_create_table(Value *args, int nargs) {
  if (nargs != 2) {
    set_err("db-create-table expects (db-create-table handle name)");
    return v_nil();
  }
  int64_t h = as_handle_arg(&args[0], "db-create-table");
  if (!h)
    return v_nil();
  const char *name = as_str_named(&args[1], "db-create-table", "table name");
  if (!name)
    return v_nil();
  Db *db = db_lookup(h, "db-create-table");
  if (!db)
    return v_nil();
  DbtSet *s = db_tables(db);
  if (!s)
    return v_nil();
  if (!dbt_set_create(s, name))
    return v_nil();
  /* The marker record, so an empty table survives a reopen: rebuild learns
   * which tables exist from the records it finds. */
  char *rk = dbt_row_key(name, DBT_MARKER_KEY);
  if (!rk)
    return v_nil();
  db_append(db, rk, "", "db-create-table");
  free(rk);
  return v_str(name);
}

/* (db-insert handle table row) -> nil. */
static Value builtin_db_insert(Value *args, int nargs) {
  if (nargs != 3) {
    set_err("db-insert expects (db-insert handle table row)");
    return v_nil();
  }
  int64_t h = as_handle_arg(&args[0], "db-insert");
  if (!h)
    return v_nil();
  const char *table = as_str_named(&args[1], "db-insert", "table name");
  if (!table)
    return v_nil();
  Db *db = db_lookup(h, "db-insert");
  if (!db)
    return v_nil();
  Value *cols = NULL;
  int ncols = 0;
  if (!dbt_row_of(&args[2], &cols, &ncols, "db-insert"))
    return v_nil();
  char *key = dbt_key_json(&cols[0], "db-insert");
  if (!key) {
    free(cols);
    return v_nil();
  }
  Value row = v_list_from_array(cols, ncols);
  Value enc = dbkv_encode(&row);
  free(cols);
  if (g_err)
    return v_nil();
  DbtTable *t = dbt_set_find(db_tables(db), table);
  if (!t) {
    set_err("db-insert: no table named '%s' in this database", table);
    v_unref(&enc);
    free(key);
    return v_nil();
  }
  char *rk = dbt_row_key(table, key);
  if (!rk) {
    v_unref(&enc);
    free(key);
    return v_nil();
  }
  db_append(db, rk, enc.u.s->data, "db-insert");
  if (!g_err)
    dbt_insert(t->tree, key, enc.u.s->data);
  v_unref(&enc);
  free(rk);
  free(key);
  return v_nil();
}

/* Collects rows for `db-all-rows`. */
typedef struct {
  Value *items;
  int n;
  int cap;
  int failed;
} DbtCollect;

static void dbt_collect_cb(const char *key, const char *val, void *ctx) {
  DbtCollect *c = (DbtCollect *)ctx;
  (void)key;
  if (strcmp(val, AINL_DB_TOMBSTONE) == 0)
    return; /* a deleted row is not a row */
  if (c->n >= c->cap) {
    c->cap = c->cap ? c->cap * 2 : 16;
    Value *grown = realloc(c->items, sizeof(Value) * (size_t)c->cap);
    if (!grown) {
      c->failed = 1;
      return;
    }
    c->items = grown;
  }
  if (!dbkv_decode(val, &c->items[c->n]))
    c->failed = 1;
  else
    c->n++;
}

/* (db-all-rows handle table) -> every row, sorted by primary key. */
static Value builtin_db_all_rows(Value *args, int nargs) {
  if (nargs != 2) {
    set_err("db-all-rows expects (db-all-rows handle table)");
    return v_nil();
  }
  int64_t h = as_handle_arg(&args[0], "db-all-rows");
  if (!h)
    return v_nil();
  const char *table = as_str_named(&args[1], "db-all-rows", "table name");
  if (!table)
    return v_nil();
  Db *db = db_lookup(h, "db-all-rows");
  if (!db)
    return v_nil();
  DbtSet *s = db_tables(db);
  DbtTable *t = s ? dbt_set_find(s, table) : NULL;
  if (!t) {
    set_err("db-all-rows: no table named '%s' in this database", table);
    return v_nil();
  }
  DbtCollect c = {NULL, 0, 0, 0};
  dbt_walk(t->tree->root, dbt_collect_cb, &c);
  if (c.failed) {
    for (int i = 0; i < c.n; i++)
      v_unref(&c.items[i]);
    free(c.items);
    set_err("db-all-rows: a row of '%s' is not readable; it was not written by "
            "db-insert",
            table);
    return v_nil();
  }
  Value out = v_list_from_array(c.items, c.n);
  free(c.items);
  return out;
}

/* (db-select handle table key) -> the row, or nil. */
static Value builtin_db_select(Value *args, int nargs) {
  if (nargs != 3) {
    set_err("db-select expects (db-select handle table key)");
    return v_nil();
  }
  int64_t h = as_handle_arg(&args[0], "db-select");
  if (!h)
    return v_nil();
  const char *table = as_str_named(&args[1], "db-select", "table name");
  if (!table)
    return v_nil();
  Db *db = db_lookup(h, "db-select");
  if (!db)
    return v_nil();
  char *key = dbt_key_json(&args[2], "db-select");
  if (!key)
    return v_nil();
  DbtSet *s = db_tables(db);
  DbtTable *t = s ? dbt_set_find(s, table) : NULL;
  if (!t) {
    set_err("db-select: no table named '%s' in this database", table);
    free(key);
    return v_nil();
  }
  const char *val = dbt_get(t->tree->root, key);
  if (!val || strcmp(val, AINL_DB_TOMBSTONE) == 0) {
    free(key);
    return v_nil();
  }
  Value out;
  if (!dbkv_decode(val, &out)) {
    set_err("db-select: the row for key %s in '%s' is not readable; it was not "
            "written by db-insert",
            key, table);
    free(key);
    return v_nil();
  }
  free(key);
  return out;
}

/* (db-delete-row handle table key) -> true if the row was there. */
static Value builtin_db_delete_row(Value *args, int nargs) {
  if (nargs != 3) {
    set_err("db-delete-row expects (db-delete-row handle table key)");
    return v_nil();
  }
  int64_t h = as_handle_arg(&args[0], "db-delete-row");
  if (!h)
    return v_nil();
  const char *table = as_str_named(&args[1], "db-delete-row", "table name");
  if (!table)
    return v_nil();
  Db *db = db_lookup(h, "db-delete-row");
  if (!db)
    return v_nil();
  char *key = dbt_key_json(&args[2], "db-delete-row");
  if (!key)
    return v_nil();
  DbtSet *s = db_tables(db);
  DbtTable *t = s ? dbt_set_find(s, table) : NULL;
  if (!t) {
    set_err("db-delete-row: no table named '%s' in this database", table);
    free(key);
    return v_nil();
  }
  const char *cur = dbt_get(t->tree->root, key);
  int existed = cur && strcmp(cur, AINL_DB_TOMBSTONE) != 0;
  if (existed) {
    char *rk = dbt_row_key(table, key);
    if (!rk) {
      free(key);
      return v_nil();
    }
    db_append(db, rk, AINL_DB_TOMBSTONE, "db-delete-row");
    if (!g_err)
      dbt_remove(t->tree, key);
    free(rk);
  }
  free(key);
  return v_bool(existed);
}

/* ---- the query layer (dbq_*) ------------------------------------------- */

/* A hand-port of ainl-core/src/dbquery.rs, read side by side with it.
 *
 * There is no FFI in this project, so this is a second implementation of one
 * specification and not a binding — deliberately, because the runtime ships as
 * a single C file the generated code is linked against, and because the point
 * of the exercise is that two engines written separately still agree.
 *
 * That agreement is the thing to preserve, and it is why every diagnostic
 * below is spelled the same as the Rust one: `dbq_aot.rs` asserts the error
 * strings equal, so a divergence is a program whose stderr depends on which
 * backend compiled it. It is also why the two unsupported-keyword tables, the
 * subset sentence and the transposition repair are duplicated rather than
 * generated — there is no build step that could hold them together, so a test
 * does. The rules the two share, in one sentence each:
 *
 *   - columns are 1-based positions, and column 1 is the primary key;
 *   - `WHERE` applies, then `ORDER BY` on the full row, then `LIMIT`;
 *   - the sort is stable over primary-key order, so `DESC` does not reverse
 *     an all-tie set;
 *   - `=` and `!=` never order, so comparing across types is a false answer
 *     rather than an error, and `<`/`>` against an unorderable value is an
 *     error naming both types;
 *   - a clause that is recognised but out of scope is refused by name, and a
 *     near miss of a legal word is offered as the fix. */

#define DBQ_WHO_MAX 32

/* The grammar, quoted in every "not supported in v1" message. One string, so
 * the message cannot disagree with the parser about what is supported. Must
 * match SUBSET in dbquery.rs byte for byte. */
#define DBQ_SUBSET                                                            \
  "SELECT <* | col, ...> FROM <table> [WHERE <col> <op> <value> "               \
  "[AND|OR <cond>]] [ORDER BY <col> [ASC|DESC]] [LIMIT <n>]"

typedef enum { DBQ_EQ, DBQ_NE, DBQ_LT, DBQ_LE, DBQ_GT, DBQ_GE } DbqOp;

typedef enum {
  DBQ_T_WORD,
  DBQ_T_INT,
  DBQ_T_FLOAT,
  DBQ_T_STR,
  DBQ_T_STAR,
  DBQ_T_COMMA,
  DBQ_T_OP,
  DBQ_T_LPAREN,
  DBQ_T_RPAREN,
  DBQ_T_END
} DbqTokKind;

typedef struct {
  DbqTokKind kind;
  /* A borrowed slice of the query for WORD/STR, or a number. Nothing here
   * outlives the query text, which the builtin holds for the whole call, so no
   * token owns anything and there is no free path to get wrong. */
  const char *text;
  int text_len;
  long long i;
  double f;
  DbqOp op;
  unsigned line, col;
} DbqTok;

typedef struct {
  int idx; /* 0-based; the language is 1-based and the conversion is in one place */
  unsigned line, col;
} DbqCol;

typedef struct {
  DbqCol left;
  DbqOp op;
  Value right;
} DbqCmp;

typedef enum { DBQ_CMP, DBQ_AND, DBQ_OR } DbqCondKind;

typedef struct DbqCond {
  DbqCondKind kind;
  DbqCmp cmp;
  struct DbqCond *a, *b;
} DbqCond;

/* A parsed query. The projections are a fixed vector because the count is
 * bounded by the query text and a program that wants a million columns is not
 * a program this layer can help; the bound is named so the refusal is a
 * sentence rather than a malloc failure. */
#define DBQ_MAX_COLS 64

typedef struct {
  int star;              /* SELECT * */
  int ncols;             /* else, the projection */
  DbqCol cols[DBQ_MAX_COLS];
  char *table; /* malloc'd; freed by dbq_query_free */
  DbqCond *filter; /* malloc'd tree, or NULL */
  int has_order;
  DbqCol order_col;
  int order_desc;
  long long limit; /* < 0 when absent */
} DbqQuery;

static void dbq_cond_free(DbqCond *c) {
  if (!c)
    return;
  dbq_cond_free(c->a);
  dbq_cond_free(c->b);
  free(c);
}

/* Keywords v1 recognises and refuses, with the words that could legally follow
 * each one here. The second half is what makes the refusal useful: a word that
 * close-matches something legal is a typo with a fix, and only a word that
 * matches nothing legal is an out-of-scope construct. Mirrors UNSUPPORTED in
 * dbquery.rs — the two lists are asserted to have the same first column by
 * dbq_aot.rs, so a keyword added to one and not the other fails there. */
typedef struct {
  const char *kw;
  const char *next[8]; /* NULL-terminated */
} DbqKw;

static const DbqKw DBQ_UNSUPPORTED[] = {
    {"JOIN", {"ON", "USING", "WHERE", "ORDER", "GROUP", "LIMIT", NULL}},
    {"INNER", {"JOIN", "ON", "WHERE", "ORDER", "LIMIT", NULL}},
    {"LEFT", {"JOIN", "ON", "USING", "WHERE", "ORDER", "LIMIT", NULL}},
    {"RIGHT", {"JOIN", "ON", "USING", "WHERE", "ORDER", "LIMIT", NULL}},
    {"FULL", {"JOIN", "ON", "USING", "WHERE", "ORDER", "LIMIT", NULL}},
    {"OUTER", {"JOIN", "ON", "USING", "WHERE", "ORDER", "LIMIT", NULL}},
    {"CROSS", {"JOIN", "ON", "USING", "WHERE", "ORDER", "LIMIT", NULL}},
    {"ON", {"WHERE", "ORDER", "GROUP", "LIMIT", NULL}},
    {"USING", {"WHERE", "ORDER", "GROUP", "LIMIT", NULL}},
    {"GROUP", {"BY", "WHERE", "ORDER", "LIMIT", NULL}},
    {"HAVING", {"WHERE", "ORDER", "LIMIT", NULL}},
    {"DISTINCT", {"FROM", "WHERE", "ORDER", "LIMIT", NULL}},
    {"UNION", {"SELECT", "WHERE", "ORDER", "LIMIT", NULL}},
    {"INTERSECT", {"SELECT", "WHERE", "ORDER", "LIMIT", NULL}},
    {"EXCEPT", {"SELECT", "WHERE", "ORDER", "LIMIT", NULL}},
    {"CASE", {"WHEN", "THEN", "ELSE", "END", "FROM", "WHERE", "LIMIT", NULL}},
    {"WHEN", {"THEN", "WHERE", "ORDER", "LIMIT", NULL}},
    {"THEN", {"WHEN", "ELSE", "END", "WHERE", "LIMIT", NULL}},
    {"ELSE", {"END", "WHERE", "ORDER", "LIMIT", NULL}},
    {"IN", {"WHERE", "ORDER", "LIMIT", NULL}},
    {"LIKE", {"WHERE", "ORDER", "LIMIT", NULL}},
    {"BETWEEN", {"WHERE", "ORDER", "LIMIT", NULL}},
    {"IS", {"NULL", "NOT", "WHERE", "ORDER", "LIMIT", NULL}},
    {"NOT", {"WHERE", "IN", "LIKE", "BETWEEN", "NULL", "ORDER", "LIMIT", NULL}},
    {"EXISTS", {"WHERE", "ORDER", "LIMIT", NULL}},
    {"OFFSET", {"ORDER", "WHERE", "LIMIT", NULL}},
    {"AS", {"WHERE", "ORDER", "LIMIT", NULL}},
    {"PRIMARY", {"KEY", "WHERE", "ORDER", "LIMIT", NULL}},
    {"KEY", {"WHERE", "ORDER", "LIMIT", NULL}},
    {"CREATE", {"TABLE", "INDEX", "FROM", "WHERE", "LIMIT", NULL}},
    {"TABLE", {"WHERE", "ORDER", "LIMIT", NULL}},
    {"DROP", {"TABLE", "INDEX", "WHERE", "ORDER", "LIMIT", NULL}},
    {"ALTER", {"TABLE", "WHERE", "ORDER", "LIMIT", NULL}},
    {"ADD", {"WHERE", "ORDER", "LIMIT", NULL}},
    {"INSERT", {"INTO", "VALUES", "FROM", "WHERE", "LIMIT", NULL}},
    {"INTO", {"VALUES", "FROM", "WHERE", "LIMIT", NULL}},
    {"VALUES", {"FROM", "WHERE", "LIMIT", NULL}},
    {"UPDATE", {"SET", "WHERE", "ORDER", "LIMIT", NULL}},
    {"DELETE", {"FROM", "WHERE", "ORDER", "LIMIT", NULL}},
    {"CAST", {"AS", "FROM", "WHERE", "LIMIT", NULL}},
    {"NULLS", {"FIRST", "LAST", "WHERE", "ORDER", "LIMIT", NULL}},
    {"FIRST", {"WHERE", "ORDER", "LIMIT", NULL}},
    {"LAST", {"WHERE", "ORDER", "LIMIT", NULL}},
};

/* The aggregates, refused with the name of the builtin that does the job. The
 * second field is that builtin, or "" when there is no equivalent. Mirrors
 * AGGREGATES in dbquery.rs. */
static const struct {
  const char *kw;
  const char *fix;
} DBQ_AGGREGATES[] = {
    {"COUNT", "db-query-count"},
    {"SUM", ""},
    {"AVG", ""},
    {"MIN", ""},
    {"MAX", ""},
};

/* The words the subset does accept. A "did you mean" is only useful if the
 * reader can type the suggestion and have it accepted. Mirrors LEGAL_WORDS. */
static const char *DBQ_LEGAL[] = {"SELECT", "FROM", "WHERE", "ORDER", "BY",
                                   "LIMIT",  "ASC",   "DESC",  "AND",   "OR",
                                   "true",   "false", "nil",   "null",  NULL};

/* A query error, built in one place. The four parts are in this order because a
 * reader who takes only the first sentence still knows the rule they broke.
 * The AOT runtime prints this whole string on stderr with no wrapper, so it
 * must be byte-identical to the Rust `sql_error`. */
static void dbq_error(const char *who, const char *sql, const DbqTok *at,
                      const char *detail, const char *suggestion) {
  if (suggestion)
    set_err("%s: at line %u, col %u: %s in the query \"%s\" \xe2\x80\x94 did you "
            "mean '%s'?",
            who, at->line, at->col, detail, sql, suggestion);
  else
    set_err("%s: at line %u, col %u: %s in the query \"%s\"", who, at->line,
            at->col, detail, sql);
}

/* The "not supported in v1" form: the refusal plus the subset sentence. */
static void dbq_unsupported(const char *who, const char *sql, const DbqTok *at,
                            const char *what) {
  char detail[1024];
  snprintf(detail, sizeof(detail),
           "%s is not supported in v1; the supported subset is: %s", what,
           DBQ_SUBSET);
  dbq_error(who, sql, at, detail, NULL);
}

/* Uppercase one byte, for the case-insensitive comparisons. AINL keywords are
 * ASCII, so a byte above 'z' is left alone rather than folded — `dbq_fold` is
 * not a Unicode case map and must not be used on display text. */
static char dbq_fold(char c) {
  return (c >= 'a' && c <= 'z') ? (char)(c - 'a' + 'A') : c;
}

/* The rolling-buffer width for the suggestion distance. Every candidate set
 * holds SQL keywords, all well under this; a longer one is skipped by
 * `dbq_close_match` rather than silently truncated. */
#define DBQ_CMP_BUF 32

/* Case-insensitive Levenshtein distance over two buffers of known length,
 * both shorter than DBQ_CMP_BUF. Two rows rather than a full table: the row
 * above is the only one the current cell reads, and this is the memory the
 * suggestion path is allowed to cost on an error path. */
static int dbq_levenshtein(const char *a, const char *b, int n, int m) {
  int prev[DBQ_CMP_BUF], cur[DBQ_CMP_BUF];
  int r, c;
  if (n > m)
    return dbq_levenshtein(b, a, m, n);
  for (c = 0; c <= m; c++)
    prev[c] = c;
  for (r = 1; r <= n; r++) {
    cur[0] = r;
    for (c = 1; c <= m; c++) {
      int cost = dbq_fold(a[r - 1]) != dbq_fold(b[c - 1]);
      int best = prev[c] + 1;
      if (cur[c - 1] + 1 < best)
        best = cur[c - 1] + 1;
      if (prev[c - 1] + cost < best)
        best = prev[c - 1] + cost;
      cur[c] = best;
    }
    for (c = 0; c <= m; c++)
      prev[c] = cur[c];
  }
  return prev[m];
}

static int dbq_streq_ci(const char *a, const char *b, int alen, int blen) {
  int i;
  if (alen != blen)
    return 0;
  for (i = 0; i < alen; i++)
    if (dbq_fold(a[i]) != dbq_fold(b[i]))
      return 0;
  return 1;
}

static int dbq_is_word_char(unsigned char c) {
  return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') ||
         (c >= '0' && c <= '9') || c == '_' || c >= 0x80;
}

static int dbq_is_digit(unsigned char c) { return c >= '0' && c <= '9'; }

static int dbq_is_space(unsigned char c) {
  return c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == 0x0C || c == 0x0B;
}

/* Is `w` (len `wl`) `want` with one pair of adjacent characters swapped?
 *
 * The one edit plain Levenshtein cannot see, and the same check dbquery.rs
 * makes: `close_match` caps at one edit for a short word, and a transposition
 * scores two, so `FORM` would otherwise get no "did you mean 'FROM'?".
 * Deliberately duplicated rather than shared — there is no FFI here, and the
 * suggestion machinery in this runtime is a different function. */
static int dbq_is_transposition(const char *w, int wl, const char *want) {
  int i, n = wl, d0 = -1, d1 = -1;
  if (n != (int)strlen(want) || n < 2)
    return 0;
  for (i = 0; i < n; i++) {
    if (dbq_fold(w[i]) == dbq_fold(want[i]))
      continue;
    if (d0 < 0)
      d0 = i;
    else if (d1 < 0)
      d1 = i;
    else
      return 0;
  }
  if (d0 < 0 || d1 < 0)
    return 0;
  return dbq_fold(w[d0]) == dbq_fold(want[d1]) &&
         dbq_fold(w[d1]) == dbq_fold(want[d0]);
}

/* The closest of `cands` to `w`, or NULL. Mirrors `close_match` in
 * suggest.rs: case-insensitive, bounded Levenshtein, a candidate must contain an
 * alphanumeric character, exact matches are skipped, and ties are broken by the
 * longer shared prefix and then alphabetically — a total order, so the answer
 * cannot depend on the order of `cands`.
 *
 * **Candidates of any length are compared.** A first version of this only
 * compared equal lengths, on the theory that a mistyped keyword keeps its
 * length. It does not: `WHER` for `WHERE` is one deletion, and the length
 * restriction made the function answer "nothing close" — so `WHER` produced a
 * bare `unexpected 'WHER'` where the interpreter said "did you mean 'WHERE'?" or
 * "'WHEN'". A reader who mistypes one character has usually also dropped one,
 * and both are one edit.
 *
 * Because the lengths now differ, the rolling buffer is sized for the longest
 * candidate and a row longer than that is skipped rather than read past. */
static const char *dbq_close_match(const char *w, int wl,
                                   const char *const *cands) {
  const char *best = NULL;
  double allow;
  int best_d = 0, best_pref = 0, c;
  if (wl < 3 || wl >= DBQ_CMP_BUF)
    return NULL;
  allow = wl * 0.34;
  if (allow < 1.0)
    allow = 1.0;
  for (c = 0; cands[c]; c++) {
    const char *cand = cands[c];
    int cl = (int)strlen(cand), d, i, pref = 0, alnum = 0;
    if (cl >= DBQ_CMP_BUF)
      continue;
    for (i = 0; i < cl; i++) {
      unsigned char ch = (unsigned char)cand[i];
      if ((ch >= '0' && ch <= '9') || (ch >= 'a' && ch <= 'z') ||
          (ch >= 'A' && ch <= 'Z'))
        alnum = 1;
    }
    if (!alnum)
      continue;
    if (dbq_streq_ci(w, cand, wl, cl))
      continue; /* an exact match is not advice */
    d = dbq_levenshtein(w, cand, wl, cl);
    if ((double)d > allow)
      continue;
    for (i = 0; i < wl && i < cl; i++)
      if (dbq_fold(w[i]) != dbq_fold(cand[i]))
        break;
    pref = i;
    if (!best || d < best_d || (d == best_d && pref > best_pref) ||
        (d == best_d && pref == best_pref && strcmp(cand, best) < 0)) {
      best = cand;
      best_d = d;
      best_pref = pref;
    }
  }
  return best;
}

/* The "not supported in v1" decision, for a bare word in a keyword position.
 * Mirrors `P::refuse_unsupported`. Returns 0 with g_err set.
 *
 * Every use of `t->text` below is `%.*s` with `t->text_len`, never `%s`. A
 * token's text is a **borrowed slice** of the query, not a C string, and `%s`
 * runs to the next NUL — which for `WHER 1=1` is the end of the whole query, so
 * the message read `unexpected 'WHER 1=1'` and pointed at a token that does not
 * exist. The other half of the fix is the type below: a slice is a struct, and
 * a struct cannot be passed to `%s` by accident. */
static int dbq_refuse_unsupported(const char *who, const char *sql,
                                  const DbqTok *t) {
  char up[64], detail[1024];
  const char *fix;
  size_t i;
  int n = t->text_len;
  if (n <= 0 || n >= (int)sizeof(up)) {
    dbq_error(who, sql, t, "unexpected token", NULL);
    return 0;
  }
  for (i = 0; i < (size_t)n; i++) {
    char c = t->text[i];
    up[i] = (char)((c >= 'a' && c <= 'z') ? c - 'a' + 'A' : c);
  }
  up[n] = 0;

  for (i = 0; i < sizeof(DBQ_AGGREGATES) / sizeof(DBQ_AGGREGATES[0]); i++) {
    if (strcmp(up, DBQ_AGGREGATES[i].kw) == 0) {
      if (DBQ_AGGREGATES[i].fix[0] == 0)
        snprintf(detail, sizeof(detail),
                 "%s is not supported in v1; the supported subset is: %s \xe2\x80\x94 "
                 "%s is an aggregate, and v1 has no aggregate expressions",
                 up, DBQ_SUBSET, up);
      else
        snprintf(detail, sizeof(detail),
                 "%s is not supported in v1; the supported subset is: %s \xe2\x80\x94 "
                 "use (%s handle \"SELECT \xe2\x80\xa6\") to count matching rows",
                 up, DBQ_SUBSET, DBQ_AGGREGATES[i].fix);
      dbq_error(who, sql, t, detail, NULL);
      return 0;
    }
  }

  for (i = 0; i < sizeof(DBQ_UNSUPPORTED) / sizeof(DBQ_UNSUPPORTED[0]); i++) {
    if (strcmp(up, DBQ_UNSUPPORTED[i].kw) == 0) {
      fix = dbq_close_match(t->text, t->text_len, DBQ_UNSUPPORTED[i].next);
      if (fix) {
        snprintf(detail, sizeof(detail), "'%s' is not supported in v1", up);
        dbq_error(who, sql, t, detail, fix);
        return 0;
      }
      snprintf(detail, sizeof(detail), "'%s'", up);
      dbq_unsupported(who, sql, t, detail);
      return 0;
    }
  }

  /* An unknown word. A near miss of an out-of-scope keyword is named as such,
   * and a near miss of a legal one is offered as the fix. Suggesting a keyword
   * without saying it is out of scope sends the reader in a circle. */
  {
    const char *refused[128];
    size_t rn = 0;
    for (i = 0; i < sizeof(DBQ_UNSUPPORTED) / sizeof(DBQ_UNSUPPORTED[0]); i++)
      refused[rn++] = DBQ_UNSUPPORTED[i].kw;
    for (i = 0; i < sizeof(DBQ_AGGREGATES) / sizeof(DBQ_AGGREGATES[0]); i++)
      refused[rn++] = DBQ_AGGREGATES[i].kw;
    refused[rn] = NULL;
    fix = dbq_close_match(t->text, t->text_len, refused);
    if (fix) {
      snprintf(detail, sizeof(detail),
               "'%.*s' is not supported in v1; the supported subset is: %s "
               "\xe2\x80\x94 did you mean '%s'?",
               t->text_len, t->text, DBQ_SUBSET, fix);
      dbq_error(who, sql, t, detail, NULL);
      return 0;
    }
    fix = dbq_close_match(t->text, t->text_len, DBQ_LEGAL);
    if (fix) {
      snprintf(detail, sizeof(detail), "unexpected '%.*s'", t->text_len, t->text);
      dbq_error(who, sql, t, detail, fix);
      return 0;
    }
  }
  snprintf(detail, sizeof(detail), "unexpected '%.*s'", t->text_len, t->text);
  dbq_error(who, sql, t, detail, NULL);
  return 0;
}

/* ---- the tokenizer ----------------------------------------------------- */

/* Bytes scanned, not characters, but the COLUMN counter advances one per
 * character so the position a reader is given is the position they can count.
 * The only place this is observably wrong is inside a multi-byte word, and
 * `dbq_utf8_len` below keeps it right there. */
static size_t dbq_utf8_len(unsigned char c) {
  if (c < 0x80)
    return 1;
  if ((c & 0xE0) == 0xC0)
    return 2;
  if ((c & 0xF0) == 0xE0)
    return 3;
  if ((c & 0xF8) == 0xF0)
    return 4;
  return 1;
}

/* The tokens, in one malloc'd array the caller frees. `Tok::End` is always the
 * last element and is never consumed, so the parser can look ahead forever
 * without a bounds check.
 *
 * Position is the token's **start**, not where the scanner happens to be when
 * the token is finished. Getting that wrong is invisible for a one-character
 * token and off by the token's whole length for a word — so `FORM` at column
 * 10 was reported at column 14, and every such message pointed past the
 * mistake at the character after it. Hence the explicit line/col argument. */
static DbqTok *dbq_tokenize(const char *who, const char *sql, int *out_n) {
  size_t cap = 32, n = 0, i = 0, len = strlen(sql);
  DbqTok *toks = (DbqTok *)malloc(cap * sizeof(DbqTok));
  unsigned line = 1, col = 1;
  if (!toks) {
    set_err("%s: out of memory", who);
    return NULL;
  }
#define DBQ_PUSH(k, tl, tc)                                                    \
  do {                                                                         \
    if (n == cap) {                                                            \
      DbqTok *grown =                                                          \
          (DbqTok *)realloc(toks, cap * 2 * sizeof(DbqTok));                  \
      if (!grown) {                                                            \
        free(toks);                                                            \
        set_err("%s: out of memory", who);                                     \
        return NULL;                                                           \
      }                                                                        \
      toks = grown;                                                            \
      cap *= 2;                                                                \
    }                                                                          \
    toks[n].kind = (k);                                                        \
    toks[n].text = NULL;                                                       \
    toks[n].text_len = 0;                                                      \
    toks[n].i = 0;                                                             \
    toks[n].f = 0.0;                                                           \
    toks[n].op = DBQ_EQ;                                                       \
    toks[n].line = (tl);                                                       \
    toks[n].col = (tc);                                                        \
    n++;                                                                       \
  } while (0)

  while (i < len) {
    unsigned char c = (unsigned char)sql[i];
    unsigned sl = line, sc = col;

    if (dbq_is_space(c)) {
      i++;
      col++;
      continue;
    }

    /* Operators, the two-character ones first: `<=` must not lex as `<` `=`. */
    if ((c == '!' || c == '<' || c == '>') && i + 1 < len && sql[i + 1] == '=') {
      DBQ_PUSH(DBQ_T_OP, sl, sc);
      toks[n - 1].op = (c == '!')   ? DBQ_NE
                       : (c == '<') ? DBQ_LE
                                    : DBQ_GE;
      i += 2;
      col += 2;
      continue;
    }
    if (c == '=' || c == '<' || c == '>') {
      DBQ_PUSH(DBQ_T_OP, sl, sc);
      toks[n - 1].op = (c == '=')   ? DBQ_EQ
                       : (c == '<') ? DBQ_LT
                                    : DBQ_GT;
      i++;
      col++;
      continue;
    }
    if (c == '*') {
      DBQ_PUSH(DBQ_T_STAR, sl, sc);
      i++;
      col++;
      continue;
    }
    if (c == ',') {
      DBQ_PUSH(DBQ_T_COMMA, sl, sc);
      i++;
      col++;
      continue;
    }
    if (c == '(') {
      DBQ_PUSH(DBQ_T_LPAREN, sl, sc);
      i++;
      col++;
      continue;
    }
    if (c == ')') {
      DBQ_PUSH(DBQ_T_RPAREN, sl, sc);
      i++;
      col++;
      continue;
    }

    /* A quoted string, either quote style, with no escapes — the AINL lexer
     * has already processed the escapes in the text this receives. */
    if (c == '\'' || c == '"') {
      char quote = (char)c;
      size_t start;
      i++;
      col++;
      start = i;
      while (i < len && sql[i] != quote) {
        if (sql[i] == '\n') {
          line++;
          col = 1;
        } else {
          col++;
        }
        i++;
      }
      if (i >= len) {
        DbqTok at = {DBQ_T_END, NULL, 0, 0, 0.0, DBQ_EQ, sl, sc};
        char detail[256];
        snprintf(detail, sizeof(detail),
                 "a string literal opened with %c was never closed", quote);
        free(toks);
        dbq_error(who, sql, &at, detail, NULL);
        return NULL;
      }
      DBQ_PUSH(DBQ_T_STR, sl, sc);
      toks[n - 1].text = sql + start;
      toks[n - 1].text_len = (int)(i - start);
      i++; /* the closing quote */
      col++;
      continue;
    }

    /* A number: an integer, or a decimal with a fraction. A leading `-` only
     * when a digit follows, so `a-1` is still two tokens. */
    if (dbq_is_digit(c) ||
        (c == '-' && i + 1 < len && dbq_is_digit((unsigned char)sql[i + 1]))) {
      size_t start = i;
      int is_float = 0;
      char buf[64];
      size_t ndig;
      if (c == '-')
        i++;
      while (i < len && dbq_is_digit((unsigned char)sql[i])) {
        i++;
        col++;
      }
      if (i < len && sql[i] == '.' && i + 1 < len &&
          dbq_is_digit((unsigned char)sql[i + 1])) {
        is_float = 1;
        i++;
        col++;
        while (i < len && dbq_is_digit((unsigned char)sql[i])) {
          i++;
          col++;
        }
      }
      /* `1abc`, `1.2.3` and `1e9` are one mistake. The position is the *running*
       * `col` — that is, the offending character itself, which is what the
       * message names — not `sl`/`sc` at the start of the number. The comment
       * here used to claim the opposite, and the C port was written to match the
       * comment, so both engines pointed one column to the left of the `a`. */
      if (i < len && (dbq_is_word_char((unsigned char)sql[i]) || sql[i] == '.')) {
        DbqTok at = {DBQ_T_END, NULL, 0, 0, 0.0, DBQ_EQ, line, col};
        char detail[256];
        char bad[2] = {sql[i], 0};
        snprintf(detail, sizeof(detail),
                 "a number cannot be followed by '%s'", bad);
        free(toks);
        dbq_error(who, sql, &at, detail, NULL);
        return NULL;
      }
      ndig = i - start;
      if (ndig >= sizeof(buf)) {
        DbqTok at = {DBQ_T_END, NULL, 0, 0, 0.0, DBQ_EQ, sl, sc};
        char detail[256];
        snprintf(detail, sizeof(detail),
                 "the number is too long to read (at most %d digits)",
                 (int)sizeof(buf) - 1);
        free(toks);
        dbq_error(who, sql, &at, detail, NULL);
        return NULL;
      }
      memcpy(buf, sql + start, ndig);
      buf[ndig] = 0;
      DBQ_PUSH(is_float ? DBQ_T_FLOAT : DBQ_T_INT, sl, sc);
      toks[n - 1].text = sql + start;
      toks[n - 1].text_len = (int)ndig;
      if (is_float) {
        toks[n - 1].f = strtod(buf, NULL);
      } else {
        char *endp = NULL;
        long long v = strtoll(buf, &endp, 10);
        if (!endp || *endp) {
          DbqTok at = {DBQ_T_END, NULL, 0, 0, 0.0, DBQ_EQ, sl, sc};
          char detail[256];
          snprintf(detail, sizeof(detail),
                   "the integer %s does not fit in a 64-bit signed integer", buf);
          free(toks);
          dbq_error(who, sql, &at, detail, NULL);
          return NULL;
        }
        toks[n - 1].i = v;
      }
      continue;
    }

    /* A bare word. A word is a keyword or a name depending on where it
     * appears, so nothing is classified here. */
    if (dbq_is_word_char(c)) {
      size_t start = i;
      while (i < len && dbq_is_word_char((unsigned char)sql[i])) {
        i += dbq_utf8_len((unsigned char)sql[i]);
        col++;
      }
      DBQ_PUSH(DBQ_T_WORD, sl, sc);
      toks[n - 1].text = sql + start;
      toks[n - 1].text_len = (int)(i - start);
      continue;
    }

    {
      DbqTok at = {DBQ_T_END, NULL, 0, 0, 0.0, DBQ_EQ, sl, sc};
      char detail[256];
      char bad[2] = {(char)c, 0};
      snprintf(detail, sizeof(detail),
               "'%s' is not part of the query language", bad);
      free(toks);
      dbq_error(who, sql, &at, detail, NULL);
      return NULL;
    }
  }
#undef DBQ_PUSH
  /* The sentinel, appended directly: the macro is already undefined. `Tok::End`
   * is never consumed, so every lookahead in the parser is in bounds. */
  if (n == cap) {
    DbqTok *grown = (DbqTok *)realloc(toks, cap * 2 * sizeof(DbqTok));
    if (!grown) {
      free(toks);
      set_err("%s: out of memory", who);
      return NULL;
    }
    toks = grown;
    cap *= 2;
  }
  toks[n].kind = DBQ_T_END;
  toks[n].text = NULL;
  toks[n].text_len = 0;
  toks[n].i = 0;
  toks[n].f = 0.0;
  toks[n].op = DBQ_EQ;
  toks[n].line = line;
  toks[n].col = col;
  n++;
  *out_n = (int)n;
  return toks;
}

/* How a token is named inside a message. Caller frees. */
static char *dbq_describe(const DbqTok *t) {
  static char buf[256];
  switch (t->kind) {
  case DBQ_T_WORD:
    snprintf(buf, sizeof(buf), "'%.*s'", t->text_len, t->text);
    break;
  case DBQ_T_INT:
    snprintf(buf, sizeof(buf), "%lld", t->i);
    break;
  case DBQ_T_FLOAT:
    snprintf(buf, sizeof(buf), "%g", t->f);
    break;
  case DBQ_T_STR:
    snprintf(buf, sizeof(buf), "'%.*s'", t->text_len, t->text);
    break;
  case DBQ_T_STAR:
    snprintf(buf, sizeof(buf), "'*'");
    break;
  case DBQ_T_COMMA:
    snprintf(buf, sizeof(buf), "','");
    break;
  case DBQ_T_OP:
    snprintf(buf, sizeof(buf), "'%s'",
             t->op == DBQ_EQ   ? "="
             : t->op == DBQ_NE ? "!="
             : t->op == DBQ_LT ? "<"
             : t->op == DBQ_LE ? "<="
             : t->op == DBQ_GT ? ">"
                               : ">=");
    break;
  case DBQ_T_LPAREN:
    snprintf(buf, sizeof(buf), "'('");
    break;
  case DBQ_T_RPAREN:
    snprintf(buf, sizeof(buf), "')'");
    break;
  default:
    snprintf(buf, sizeof(buf), "the end of the query");
    break;
  }
  return buf;
}

/* ---- the parser -------------------------------------------------------- */

typedef struct {
  const char *who;
  const char *sql;
  DbqTok *toks;
  int n;
  int i;
} DbqP;

static DbqTok *dbq_peek(DbqP *p) {
  return &p->toks[p->i < p->n - 1 ? p->i : p->n - 1];
}

/* Consume `word` if it is next, case-insensitively. */
static int dbq_eat(DbqP *p, const char *word) {
  DbqTok *t = dbq_peek(p);
  if (t->kind == DBQ_T_WORD &&
      dbq_streq_ci(t->text, word, t->text_len, (int)strlen(word))) {
    p->i++;
    return 1;
  }
  return 0;
}

/* A keyword position. The transposition check is here rather than only in
 * `close_match` for the reason documented on `dbq_is_transposition`. */
static int dbq_keyword(DbqP *p, const char *word) {
  DbqTok *t = dbq_peek(p);
  DbqTok copy;
  if (dbq_eat(p, word))
    return 1;
  copy = *t;
  if (copy.kind == DBQ_T_END) {
    char detail[1024];
    snprintf(detail, sizeof(detail),
             "the query ended, but %s is required; the supported subset is: %s",
             word, DBQ_SUBSET);
    dbq_error(p->who, p->sql, &copy, detail, NULL);
    return 0;
  }
  if (copy.kind == DBQ_T_WORD &&
      dbq_is_transposition(copy.text, copy.text_len, word)) {
    char detail[256];
    snprintf(detail, sizeof(detail), "unexpected '%.*s'", copy.text_len,
             copy.text);
    dbq_error(p->who, p->sql, &copy, detail, word);
    return 0;
  }
  return dbq_refuse_unsupported(p->who, p->sql, &copy);
}

static int dbq_is_known_keyword(const char *w, int wl) {
  char up[64];
  size_t i;
  if (wl <= 0 || wl >= (int)sizeof(up))
    return 0;
  for (i = 0; i < (size_t)wl; i++) {
    char c = w[i];
    up[i] = (char)((c >= 'a' && c <= 'z') ? c - 'a' + 'A' : c);
  }
  up[wl] = 0;
  for (i = 0; i < sizeof(DBQ_UNSUPPORTED) / sizeof(DBQ_UNSUPPORTED[0]); i++)
    if (strcmp(up, DBQ_UNSUPPORTED[i].kw) == 0)
      return 1;
  for (i = 0; i < sizeof(DBQ_AGGREGATES) / sizeof(DBQ_AGGREGATES[0]); i++)
    if (strcmp(up, DBQ_AGGREGATES[i].kw) == 0)
      return 1;
  return 0;
}

/* A column reference: a 1-based position. */
static int dbq_column(DbqP *p, DbqCol *out) {
  DbqTok *t = dbq_peek(p);
  DbqTok copy = *t;
  if (copy.kind == DBQ_T_INT) {
    if (copy.i < 1) {
      char detail[256];
      snprintf(detail, sizeof(detail),
               "column %lld does not exist; columns are numbered from 1", copy.i);
      dbq_error(p->who, p->sql, &copy, detail, NULL);
      return 0;
    }
    out->idx = (int)(copy.i - 1);
    out->line = copy.line;
    out->col = copy.col;
    p->i++;
    return 1;
  }
  if (copy.kind == DBQ_T_WORD) {
    if (dbq_is_known_keyword(copy.text, copy.text_len))
      return dbq_refuse_unsupported(p->who, p->sql, &copy);
    {
      char detail[512];
      snprintf(detail, sizeof(detail),
               "'%.*s' is not a column: a row in this database is a list, so "
               "columns are numbered from 1 and 1 is the primary key",
               copy.text_len, copy.text);
      dbq_error(p->who, p->sql, &copy, detail, NULL);
    }
    return 0;
  }
  {
    char detail[512];
    char *d = dbq_describe(&copy);
    snprintf(detail, sizeof(detail), "expected a column number, got %s", d);
    dbq_error(p->who, p->sql, &copy, detail, NULL);
  }
  return 0;
}

/* The right-hand side of a comparison: a number, a quoted string, or one of
 * the four literal words. A bare word that is none of those is a column, and
 * comparing two columns is not in the subset. */
static int dbq_literal(DbqP *p, Value *out) {
  DbqTok copy = *dbq_peek(p);
  switch (copy.kind) {
  case DBQ_T_INT:
    *out = v_int((int64_t)copy.i);
    p->i++;
    return 1;
  case DBQ_T_FLOAT:
    *out = v_float(copy.f);
    p->i++;
    return 1;
  case DBQ_T_STR: {
    char *s = (char *)malloc((size_t)copy.text_len + 1);
    if (!s) {
      set_err("%s: out of memory", p->who);
      return 0;
    }
    memcpy(s, copy.text, (size_t)copy.text_len);
    s[copy.text_len] = 0;
    *out = v_str_take(s);
    p->i++;
    return 1;
  }
  case DBQ_T_LPAREN: {
    char detail[256];
    snprintf(detail, sizeof(detail), "a subquery as a value");
    dbq_unsupported(p->who, p->sql, &copy, detail);
    return 0;
  }
  case DBQ_T_WORD: {
    if (dbq_streq_ci(copy.text, "true", copy.text_len, 4)) {
      *out = v_bool(1);
      p->i++;
      return 1;
    }
    if (dbq_streq_ci(copy.text, "false", copy.text_len, 5)) {
      *out = v_bool(0);
      p->i++;
      return 1;
    }
    if (dbq_streq_ci(copy.text, "nil", copy.text_len, 3) ||
        dbq_streq_ci(copy.text, "null", copy.text_len, 4)) {
      *out = v_nil();
      p->i++;
      return 1;
    }
    if (dbq_is_known_keyword(copy.text, copy.text_len))
      return dbq_refuse_unsupported(p->who, p->sql, &copy);
    {
      char detail[512];
      snprintf(detail, sizeof(detail),
               "'%.*s' is not a value: the right-hand side of a comparison is a "
               "number, a quoted string, true, false or nil, and a column name "
               "is not accepted here (only a column on the left)",
               copy.text_len, copy.text);
      dbq_error(p->who, p->sql, &copy, detail, NULL);
    }
    return 0;
  }
  default: {
    char detail[512];
    char *d = dbq_describe(&copy);
    snprintf(detail, sizeof(detail),
             "expected a value \xe2\x80\x94 a number, a quoted string, true, "
             "false or nil \xe2\x80\x94 got %s",
             d);
    dbq_error(p->who, p->sql, &copy, detail, NULL);
    return 0;
  }
  }
}

static int dbq_comparison(DbqP *p, DbqCmp *out) {
  DbqTok copy;
  if (!dbq_column(p, &out->left))
    return 0;
  copy = *dbq_peek(p);
  if (copy.kind == DBQ_T_OP) {
    out->op = copy.op;
    p->i++;
  } else if (copy.kind == DBQ_T_WORD) {
    return dbq_refuse_unsupported(p->who, p->sql, &copy);
  } else {
    char detail[512];
    char *d = dbq_describe(&copy);
    snprintf(detail, sizeof(detail),
             "expected one of '=', '!=', '<', '<=', '>', '>=' after the column, "
             "got %s",
             d);
    dbq_error(p->who, p->sql, &copy, detail, NULL);
    return 0;
  }
  return dbq_literal(p, &out->right);
}

static DbqCond *dbq_cond_new(DbqCondKind k) {
  DbqCond *c = (DbqCond *)calloc(1, sizeof(DbqCond));
  if (c)
    c->kind = k;
  return c;
}

/* `cond := cmp (AND|OR cmp)*`, with AND binding tighter — SQL's own rule, and
 * the only one that needs no parentheses, which v1 does not have. */
static int dbq_condition(DbqP *p, DbqCond **out) {
  DbqCond *left = dbq_cond_new(DBQ_CMP);
  if (!left) {
    set_err("%s: out of memory", p->who);
    return 0;
  }
  if (!dbq_comparison(p, &left->cmp)) {
    free(left);
    return 0;
  }
  for (;;) {
    if (dbq_eat(p, "AND")) {
      DbqCond *right = dbq_cond_new(DBQ_CMP);
      DbqCond *node;
      if (!right) {
        set_err("%s: out of memory", p->who);
        dbq_cond_free(left);
        return 0;
      }
      if (!dbq_comparison(p, &right->cmp)) {
        free(right);
        dbq_cond_free(left);
        return 0;
      }
      node = dbq_cond_new(DBQ_AND);
      if (!node) {
        set_err("%s: out of memory", p->who);
        free(right);
        dbq_cond_free(left);
        return 0;
      }
      node->a = left;
      node->b = right;
      left = node;
      continue;
    }
    if (dbq_eat(p, "OR")) {
      /* Everything to the right of OR is a full AND-chain, so `a OR b AND c` is
       * `a OR (b AND c)`. Recursing into `dbq_condition` (rather than a separate
       * and-chain function) is what gives that: the recursive call consumes AND
       * to the end of the clause and stops before the next OR. */
      DbqCond *right, *node;
      if (!dbq_condition(p, &right)) {
        dbq_cond_free(left);
        return 0;
      }
      node = dbq_cond_new(DBQ_OR);
      if (!node) {
        set_err("%s: out of memory", p->who);
        dbq_cond_free(left);
        dbq_cond_free(right);
        return 0;
      }
      node->a = left;
      node->b = right;
      /* `*out` is written here as well as in the exit below. Omitting it on this
       * path left the caller's filter pointer as NULL — the query then ran with
       * no WHERE at all and returned every row, which is the worst possible
       * failure for a filter: it looks like an answer. */
      *out = node;
      return 1;
    }
    *out = left;
    return 1;
  }
}

static void dbq_query_free(DbqQuery *q) {
  free(q->table);
  dbq_cond_free(q->filter);
  q->table = NULL;
  q->filter = NULL;
}

/* Parse `sql` into `out`, or set g_err. Caller calls `dbq_query_free`. */
static int dbq_parse(const char *who, const char *sql, DbqQuery *out) {
  DbqP p;
  int n = 0, ncols = 0;
  DbqTok *toks = dbq_tokenize(who, sql, &n);
  DbqTok copy;

  memset(out, 0, sizeof(*out));
  out->limit = -1;
  if (!toks)
    return 0;
  p.who = who;
  p.sql = sql;
  p.toks = toks;
  p.n = n;
  p.i = 0;

  if (!dbq_keyword(&p, "SELECT"))
    goto fail;

  /* The projection. */
  if (dbq_peek(&p)->kind == DBQ_T_STAR) {
    out->star = 1;
    p.i++;
  } else {
    DbqCol c;
    /* A query that is only `SELECT` has no projection, and "expected a column
     * number, got the end of the query" would send a model looking for a typo
     * that is not there — the missing word is FROM. */
    if (dbq_peek(&p)->kind == DBQ_T_END) {
      char detail[1024];
      snprintf(detail, sizeof(detail),
               "the query ended, but FROM is required; the supported subset is: %s",
               DBQ_SUBSET);
      copy = *dbq_peek(&p);
      dbq_error(who, sql, &copy, detail, NULL);
      goto fail;
    }
    if (!dbq_column(&p, &c))
      goto fail;
    out->cols[ncols++] = c;
    while (dbq_peek(&p)->kind == DBQ_T_COMMA) {
      p.i++;
      if (!dbq_column(&p, &c))
        goto fail;
      if (ncols >= DBQ_MAX_COLS) {
        char detail[256];
        copy = *dbq_peek(&p);
        snprintf(detail, sizeof(detail),
                 "a query can select at most %d columns in v1", DBQ_MAX_COLS);
        dbq_error(who, sql, &copy, detail, NULL);
        goto fail;
      }
      out->cols[ncols++] = c;
    }
    out->ncols = ncols;
  }

  if (!dbq_keyword(&p, "FROM"))
    goto fail;

  /* The table name, taken literally: no keyword check, so a table called
   * `order` or `key` is selectable. */
  copy = *dbq_peek(&p);
  if (copy.kind == DBQ_T_WORD || copy.kind == DBQ_T_STR) {
    out->table = (char *)malloc((size_t)copy.text_len + 1);
    if (!out->table) {
      set_err("%s: out of memory", who);
      goto fail;
    }
    memcpy(out->table, copy.text, (size_t)copy.text_len);
    out->table[copy.text_len] = 0;
    p.i++;
  } else if (copy.kind == DBQ_T_LPAREN) {
    dbq_unsupported(who, sql, &copy,
                    "a subquery or a parenthesised table (a join) in FROM");
    goto fail;
  } else {
    char detail[512];
    char *d = dbq_describe(&copy);
    snprintf(detail, sizeof(detail), "expected a table name after FROM, got %s",
             d);
    dbq_error(who, sql, &copy, detail, NULL);
    goto fail;
  }

  /* The clauses, in the order SQL requires them. The order is enforced, not
   * documented: the first version of this loop accepted any order, so
   * `LIMIT 1 ORDER BY 2` applied the limit before the sort it was written
   * after. */
  for (;;) {
    copy = *dbq_peek(&p);
    if (copy.kind == DBQ_T_END)
      break;

    if (dbq_eat(&p, "WHERE")) {
      if (out->has_order || out->limit >= 0) {
        char detail[512];
        char *d = dbq_describe(&copy);
        snprintf(detail, sizeof(detail),
                 "WHERE comes before ORDER BY and LIMIT in a query, so %s was "
                 "written too late",
                 d);
        dbq_error(who, sql, &copy, detail, NULL);
        goto fail;
      }
      if (out->filter) {
        dbq_error(who, sql, &copy, "a query can have only one WHERE clause",
                  NULL);
        goto fail;
      }
      if (!dbq_condition(&p, &out->filter))
        goto fail;
      continue;
    }
    if (dbq_eat(&p, "ORDER")) {
      if (out->limit >= 0) {
        char detail[512];
        char *d = dbq_describe(&copy);
        snprintf(detail, sizeof(detail),
                 "ORDER BY comes before LIMIT in a query, so %s was written too "
                 "late",
                 d);
        dbq_error(who, sql, &copy, detail, NULL);
        goto fail;
      }
      if (out->has_order) {
        dbq_error(who, sql, &copy, "a query can have only one ORDER BY clause",
                  NULL);
        goto fail;
      }
      /* `ORDER 2` is a missing `BY`, and "unexpected 2" sends the reader
       * looking for a bad number rather than a missing word. */
      if (!dbq_eat(&p, "BY")) {
        DbqTok inner = *dbq_peek(&p);
        if (inner.kind == DBQ_T_INT || inner.kind == DBQ_T_STAR ||
            inner.kind == DBQ_T_WORD) {
          char detail[512];
          char *d = dbq_describe(&inner);
          snprintf(detail, sizeof(detail), "ORDER must be followed by BY; got %s",
                   d);
          dbq_error(who, sql, &inner, detail, "BY");
          goto fail;
        }
        if (!dbq_refuse_unsupported(who, sql, &inner))
          goto fail;
      }
      if (!dbq_column(&p, &out->order_col))
        goto fail;
      out->order_desc = dbq_eat(&p, "DESC");
      if (!out->order_desc)
        dbq_eat(&p, "ASC");
      out->has_order = 1;
      continue;
    }
    if (dbq_eat(&p, "LIMIT")) {
      if (out->limit >= 0) {
        dbq_error(who, sql, &copy, "a query can have only one LIMIT clause",
                  NULL);
        goto fail;
      }
      copy = *dbq_peek(&p);
      if (copy.kind == DBQ_T_INT && copy.i >= 0) {
        out->limit = copy.i;
        p.i++;
      } else if (copy.kind == DBQ_T_INT) {
        char detail[256];
        snprintf(detail, sizeof(detail),
                 "LIMIT %lld is negative; a limit is a count of rows", copy.i);
        dbq_error(who, sql, &copy, detail, NULL);
        goto fail;
      } else {
        char detail[512];
        char *d = dbq_describe(&copy);
        snprintf(detail, sizeof(detail), "expected a row count after LIMIT, got %s",
                 d);
        dbq_error(who, sql, &copy, detail, NULL);
        goto fail;
      }
      continue;
    }
    if (!dbq_refuse_unsupported(who, sql, &copy))
      goto fail;
  }

  free(toks);
  return 1;

fail:
  free(toks);
  dbq_query_free(out);
  return 0;
}

/* ---- evaluation -------------------------------------------------------- */

/* The order of two values, for the four ordering operators and for ORDER BY.
 *
 * The rules are the language's own (see `collections::default_compare`), because
 * a query that ordered differently from `sort` would be a second comparison in
 * the language. Numbers compare across int and float; strings compare by
 * **bytes**, so `memcmp` here is the same total order the Rust `compare_bytes`
 * gives and the two engines agree without a collation table; booleans order
 * false < true; nil orders equal to itself and nothing else.
 *
 * On failure the reason is written into `why` in the *Rust* form — including
 * the `db-query: ` prefix and both type names — so the message a caller wraps
 * reads identically on both engines. That is the whole point of `why` being a
 * buffer the caller supplies rather than a `const char *` this file owns: the
 * two implementations cannot drift without the AOT test noticing.
 *
 * Returns -1/0/1, or 2 for "not comparable". */
static int dbq_order_of(Value *a, Value *b, char *why, size_t whyn) {
  int a_num = (a->tag == V_INT || a->tag == V_FLOAT);
  int b_num = (b->tag == V_INT || b->tag == V_FLOAT);
  if (a_num && b_num) {
    double x = as_f64(a), y = as_f64(b);
    if (x != x || y != y) { /* NaN */
      snprintf(why, whyn, "db-query: cannot order NaN");
      return 2;
    }
    return x < y ? -1 : (x > y ? 1 : 0);
  }
  if (a->tag == V_STR && b->tag == V_STR) {
    size_t n = (size_t)(a->u.s->len < b->u.s->len ? a->u.s->len : b->u.s->len);
    int r = memcmp(a->u.s->data, b->u.s->data, n);
    if (r != 0)
      return r < 0 ? -1 : 1;
    if (a->u.s->len == b->u.s->len)
      return 0;
    return a->u.s->len < b->u.s->len ? -1 : 1;
  }
  if (a->tag == V_BOOL && b->tag == V_BOOL)
    return a->u.b - b->u.b;
  if (a->tag == V_NIL && b->tag == V_NIL)
    return 0;
  snprintf(why, whyn, "db-query: cannot order a %s and a %s", type_name(a),
           type_name(b));
  return 2;
}

/* Element at `idx` of a list, or nil. Mirrors `ConsCell::nth` and the
 * out-of-range-is-nil convention, so a projection and a WHERE see the same
 * thing on both engines. */
static Value *dbq_nth(ConsCell *cell, int idx) {
  ConsCell *cur = cell;
  int i = 0;
  while (cur && cur->len > 0) {
    if (i == idx)
      return &cur->head;
    cur = cur->tail;
    i++;
  }
  return NULL;
}

static int dbq_eval_cmp(const char *who, DbqCmp *c, ConsCell *row) {
  Value left = v_nil(), right = c->right;
  char why[256];
  int ord;
  Value *found = dbq_nth(row, c->left.idx);
  if (found) {
    left = *found;
    v_ref(&left);
  }
  ord = dbq_order_of(&left, &right, why, sizeof(why));
  v_unref(&left);
  if (ord == 2) {
    /* `=` and `!=` do not order: `(= "a" 1)` is a false answer, not a type
     * error, because that is what `=` already does everywhere else. */
    if (c->op == DBQ_EQ || c->op == DBQ_NE) {
      Value *lv = dbq_nth(row, c->left.idx);
      int eq = lv ? values_eq(lv, &right) : values_eq(&left, &right);
      return c->op == DBQ_NE ? !eq : eq;
    }
    set_err("%s: at line %u, col %u: WHERE column %d is a %s and the value "
            "compared with it is a %s \xe2\x80\x94 a column that is compared "
            "with <, <=, > or >= has to hold one type in every row (%s)",
            who, c->left.line, c->left.col, c->left.idx + 1, type_name(&left),
            type_name(&right), why);
    return -1;
  }
  switch (c->op) {
  case DBQ_EQ:
    return ord == 0;
  case DBQ_NE:
    return ord != 0;
  case DBQ_LT:
    return ord < 0;
  case DBQ_LE:
    return ord <= 0;
  case DBQ_GT:
    return ord > 0;
  default:
    return ord >= 0;
  }
}

static int dbq_eval_cond(const char *who, DbqCond *c, ConsCell *row) {
  switch (c->kind) {
  case DBQ_CMP:
    return dbq_eval_cmp(who, &c->cmp, row);
  case DBQ_AND: {
    int a = dbq_eval_cond(who, c->a, row);
    if (a < 0)
      return -1;
    if (!a)
      return 0;
    return dbq_eval_cond(who, c->b, row);
  }
  default: {
    int a = dbq_eval_cond(who, c->a, row);
    if (a < 0)
      return -1;
    if (a)
      return 1;
    return dbq_eval_cond(who, c->b, row);
  }
  }
}

/* Bottom-up stable merge, mirroring `sort_merge` and the Rust `merge_by`.
 *
 * All three are written the same way rather than one delegating to another,
 * because the property that matters is **stability**: equal keys must keep
 * primary-key order, or an ORDER BY on a duplicated value would answer
 * differently on two engines that both pass every equality test. A host
 * `qsort` is not an option for the same reason the runtime's `sort` does not
 * use one. */
static int dbq_merge_by(const char *who, Value *items, int n, int order_idx,
                        int desc, unsigned sort_line, unsigned sort_col) {
  Value *tmp = NULL;
  int width, start;
  if (n < 2)
    return 1;
  tmp = (Value *)malloc(sizeof(Value) * (size_t)n);
  if (!tmp) {
    set_err("%s: out of memory", who);
    return 0;
  }
  for (width = 1; width < n; width *= 2) {
    for (start = 0; start < n; start += 2 * width) {
      int mid = start + width < n ? start + width : n;
      int end = start + 2 * width < n ? start + 2 * width : n;
      int l = start, r = mid, k = start;
      while (l < mid && r < end) {
        Value *av = dbq_nth(items[l].u.l, order_idx);
        Value *bv = dbq_nth(items[r].u.l, order_idx);
        Value an = v_nil(), bn = v_nil();
        char why[256];
        int ord;
        if (av) {
          an = *av;
          v_ref(&an);
        }
        if (bv) {
          bn = *bv;
          v_ref(&bn);
        }
        ord = dbq_order_of(&an, &bn, why, sizeof(why));
        if (ord == 2) {
          /* Unorderable in the sort itself. A column that holds *different
           * types in different rows* is the one failure the per-row shape
           * check in `dbq_execute` cannot see, and it has to be an error here
           * rather than a silent tie — a silent tie would leave the two rows in
           * B-tree order, which reads as a successful sort. */
          set_err("%s: at line %u, col %u: ORDER BY column %d is a %s and "
                  "cannot be ordered (%s)",
                  who, sort_line, sort_col, order_idx + 1, type_name(&an), why);
          v_unref(&an);
          v_unref(&bn);
          free(tmp);
          return 0;
        }
        v_unref(&an);
        v_unref(&bn);
        /* `ord <= 0` takes from the left on ties — the stability rule. */
        if (desc)
          ord = -ord;
        if (ord <= 0)
          tmp[k++] = items[l++];
        else
          tmp[k++] = items[r++];
      }
      while (l < mid)
        tmp[k++] = items[l++];
      while (r < end)
        tmp[k++] = items[r++];
    }
    for (start = 0; start < n; start++)
      items[start] = tmp[start];
  }
  free(tmp);
  return 1;
}

/* The rows a query produces: a list of lists. `want_count` skips building the
 * rows and answers with the number only, which is what `db-query-count` needs
 * and is why it can be cheaper on a large table for no extra code path. */
/* Collects rows for the scan half of `dbq_execute`. A private copy of
 * `DbtCollect` rather than a reuse of that struct with `dbt_collect_cb`: the
 * shared callback cannot report a decode failure with a message naming
 * *this* builtin, and a row that is not readable is a refusal whose text has to
 * say which query hit it. The Rust side decodes each row itself for the same
 * reason. */
struct DbqCollect {
  Value *items;
  int n;
  int cap;
  int failed;
  int oom;
};

static void dbq_collect_push(struct DbqCollect *c, const char *val) {
  if (c->n >= c->cap) {
    Value *grown;
    c->cap = c->cap ? c->cap * 2 : 16;
    grown = (Value *)realloc(c->items, sizeof(Value) * (size_t)c->cap);
    if (!grown) {
      c->oom = 1;
      return;
    }
    c->items = grown;
  }
  if (!dbkv_decode(val, &c->items[c->n]))
    c->failed = 1;
  else
    c->n++;
}

static void dbq_collect_cb(const char *key, const char *val, void *ctx) {
  struct DbqCollect *c = (struct DbqCollect *)ctx;
  (void)key;
  if (strcmp(val, AINL_DB_TOMBSTONE) == 0)
    return; /* a deleted row is not a row */
  dbq_collect_push(c, val);
}

static Value dbq_execute(Db *db, const char *who, const char *sql, int want_count) {
  DbqQuery q;
  DbtSet *s;
  DbtTable *t;
  Value *rows = NULL;
  int nrows = 0, i;
  Value out = v_nil();

  if (!dbq_parse(who, sql, &q))
    return v_nil();

  s = db_tables(db);
  t = s ? dbt_set_find(s, q.table) : NULL;
  if (!t) {
    set_err("%s: no table named '%s' in this database", who, q.table);
    dbq_query_free(&q);
    return v_nil();
  }

  /* The rows to consider: one point lookup, or the tree's own walk — the
   * latter already in primary-key order, which is what makes the sort below
   * stable *and* what makes an unordered query's output deterministic. */
  if (q.filter && q.filter->kind == DBQ_CMP && q.filter->cmp.left.idx == 0 &&
      q.filter->cmp.op == DBQ_EQ &&
      q.filter->cmp.right.tag != V_LIST && q.filter->cmp.right.tag != V_MAP) {
    /* The one shape that reads a single key: `WHERE 1 = <scalar>`. Column 1
     * is the primary key, so this is the B-tree lookup `db-select` does, and
     * the key text is byte-identical to the one `db-insert` stored because both
     * go through `dbt_key_json`. Everything else walks. */
    char *key = dbt_key_json(&q.filter->cmp.right, who);
    const char *val;
    if (!key) {
      dbq_query_free(&q);
      return v_nil();
    }
    val = dbt_get(t->tree->root, key);
    if (val && strcmp(val, AINL_DB_TOMBSTONE) != 0) {
      struct DbqCollect c = {NULL, 0, 0, 0, 0};
      dbq_collect_push(&c, val);
      if (c.oom) {
        set_err("%s: out of memory", who);
      } else if (c.failed) {
        set_err("%s: the row for key %s in '%s' is not readable; it was not "
                "written by db-insert",
                who, key, q.table);
      } else {
        rows = c.items;
        nrows = c.n;
      }
    }
    free(key);
  } else {
    struct DbqCollect c = {NULL, 0, 0, 0, 0};
    dbt_walk(t->tree->root, dbq_collect_cb, &c);
    if (c.oom) {
      free(c.items);
      set_err("%s: out of memory", who);
      dbq_query_free(&q);
      return v_nil();
    }
    if (c.failed) {
      for (i = 0; i < c.n; i++)
        v_unref(&c.items[i]);
      free(c.items);
      set_err("%s: a row of '%s' is not readable; it was not written by "
              "db-insert",
              who, q.table);
      dbq_query_free(&q);
      return v_nil();
    }
    rows = c.items;
    nrows = c.n;
  }

  if (g_err) {
    for (i = 0; i < nrows; i++)
      v_unref(&rows[i]);
    free(rows);
    dbq_query_free(&q);
    return v_nil();
  }

  /* The filter, in one pass that compacts in place.
   *
   * One pass, not "count then compact": a two-pass version evaluates every
   * comparison twice, so a WHERE whose value is unorderable in the *last* row
   * would report an error the first pass missed, and the double evaluation is a
   * second thing to keep in step with the Rust side. Compacting as we go also
   * means the ORDER BY shape check below sees only rows the filter kept —
   * which is what the Rust `full` vector is, and why the one-row case there
   * needed its own pass. */
  if (q.filter) {
    int w = 0;
    for (i = 0; i < nrows; i++) {
      int r = dbq_eval_cond(who, q.filter, rows[i].u.l);
      if (r < 0) {
        int j;
        for (j = w; j < nrows; j++)
          v_unref(&rows[j]);
        free(rows);
        dbq_query_free(&q);
        return v_nil();
      }
      if (r) {
        rows[w++] = rows[i];
      } else {
        v_unref(&rows[i]);
      }
    }
    nrows = w;
  }

  /* Every row must have the ORDER BY column, checked before the sort.
   *
   * A comparator alone cannot catch a column out of range on *every* row: nil
   * orders equal to nil, so the sort would "succeed" and return primary-key
   * order. It also cannot catch it when the filter left one row, because a
   * merge of one element never calls a comparator — which is why this is a
   * separate pass over `rows` and not a rule inside the comparator. */
  if (q.has_order) {
    for (i = 0; i < nrows; i++) {
      if (!dbq_nth(rows[i].u.l, q.order_col.idx)) {
        set_err("%s: at line %u, col %u: ORDER BY column %d is nil in every row "
                "of '%s' \xe2\x80\x94 a row in this database is a list, and no "
                "row here is that long",
                who, q.order_col.line, q.order_col.col, q.order_col.idx + 1,
                q.table);
        for (i = 0; i < nrows; i++)
          v_unref(&rows[i]);
        free(rows);
        dbq_query_free(&q);
        return v_nil();
      }
    }
    if (nrows > 1 &&
        !dbq_merge_by(who, rows, nrows, q.order_col.idx, q.order_desc,
                       q.order_col.line, q.order_col.col)) {
      for (i = 0; i < nrows; i++)
        v_unref(&rows[i]);
      free(rows);
      dbq_query_free(&q);
      return v_nil();
    }
  }

  if (q.limit >= 0 && q.limit < nrows) {
    for (i = (int)q.limit; i < nrows; i++)
      v_unref(&rows[i]);
    nrows = (int)q.limit;
  }

  if (want_count) {
    for (i = 0; i < nrows; i++)
      v_unref(&rows[i]);
    free(rows);
    out = v_int(nrows);
    dbq_query_free(&q);
    return out;
  }

  if (q.star) {
    /* `v_list_from_array` takes ownership of the refs in the array, so `rows`
     * is handed over rather than released afterwards. */
    out = v_list_from_array(rows, nrows);
    free(rows);
    dbq_query_free(&q);
    return out;
  }

  /* The projection, into one flat array of `nrows * ncols` values, then a list
   * per row. Flat because `v_list_from_array` is the only list builder and it
   * takes a contiguous slice — so each row's cells must sit side by side, and
   * a per-row array would mean hand-rolling the cons chain the runtime already
   * has. Each cell is ref'd here and consumed by the builder that reads it, so
   * exactly one reference per cell exists at every point. */
  {
    int total = nrows * q.ncols;
    Value *flat = (Value *)malloc(sizeof(Value) * (size_t)(total ? total : 1));
    int j = 0, k;
    if (!flat) {
      set_err("%s: out of memory", who);
      for (i = 0; i < nrows; i++)
        v_unref(&rows[i]);
      free(rows);
      dbq_query_free(&q);
      return v_nil();
    }
    for (i = 0; i < nrows; i++) {
      for (k = 0; k < q.ncols; k++) {
        Value *cell = dbq_nth(rows[i].u.l, q.cols[k].idx);
        /* Out of range is nil, the convention `nth` already has. A projection
         * is expected to inherit it; an ORDER BY on the same column is a claim
         * about the table's shape and is an error, and the two are checked in
         * different places on purpose. */
        if (cell) {
          flat[j] = *cell;
          v_ref(&flat[j]);
        } else {
          flat[j] = v_nil();
        }
        j++;
      }
    }
    out = v_list_empty();
    for (i = nrows - 1; i >= 0; i--) {
      Value row = v_list_from_array(flat + i * q.ncols, q.ncols);
      ConsCell *cell = cons_cell_new(row, out.u.l);
      Value next;
      next.tag = V_LIST;
      next.u.l = cell;
      out = next;
    }
    free(flat);
  }
  free(rows);
  dbq_query_free(&q);
  return out;
}

/* (db-query handle "SELECT ...") -> the matching rows, projected. */
static Value builtin_db_query(Value *args, int nargs) {
  int64_t h;
  const char *sql;
  Db *db;
  Value out;
  if (nargs != 2) {
    set_err("db-query expects (db-query handle \"SELECT ...\")");
    return v_nil();
  }
  h = as_handle_arg(&args[0], "db-query");
  if (!h)
    return v_nil();
  sql = as_str_named(&args[1], "db-query", "str query");
  if (!sql)
    return v_nil();
  db = db_lookup(h, "db-query");
  if (!db)
    return v_nil();
  out = dbq_execute(db, "db-query", sql, 0);
  if (g_err)
    return v_nil();
  return out;
}

/* (db-query-count handle "SELECT ...") -> how many rows match, after LIMIT. */
static Value builtin_db_query_count(Value *args, int nargs) {
  int64_t h;
  const char *sql;
  Db *db;
  Value out;
  if (nargs != 2) {
    set_err("db-query-count expects (db-query-count handle \"SELECT ...\")");
    return v_nil();
  }
  h = as_handle_arg(&args[0], "db-query-count");
  if (!h)
    return v_nil();
  sql = as_str_named(&args[1], "db-query-count", "str query");
  if (!sql)
    return v_nil();
  db = db_lookup(h, "db-query-count");
  if (!db)
    return v_nil();
  out = dbq_execute(db, "db-query-count", sql, 1);
  if (g_err)
    return v_nil();
  return out;
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
    case B_JSON_PARSE:
      return builtin_json_parse(args, nargs);
    case B_JSON_SERIALIZE:
      return builtin_json_serialize(args, nargs);
    case B_TEST:
      return builtin_test(args, nargs);
    case B_SORT:
      return builtin_sort(args, nargs);
    /* Byte-oriented string primitives (Tier 3) */
    case B_SUBSTRING:
      return builtin_substring(args, nargs);
    case B_CHAR:
      return builtin_char(args, nargs);
    case B_CODE:
      return builtin_code(args, nargs);
    case B_STARTS_WITH:
      return builtin_starts_with(args, nargs);
    case B_ENDS_WITH:
      return builtin_ends_with(args, nargs);
    case B_INDEX_OF:
      return builtin_index_of(args, nargs);
    /* Tier 3 file system */
    case B_MKDIR:
      return builtin_mkdir(args, nargs);
    case B_RENAME:
      return builtin_rename(args, nargs);
    case B_COPY:
      return builtin_copy(args, nargs);
    case B_IS_DIR:
      return builtin_is_dir(args, nargs);
    case B_FILE_SIZE:
      return builtin_file_size(args, nargs);
    /* Tier 4 storage */
    case B_DB_OPEN:
      return builtin_db_open(args, nargs);
    case B_DB_PUT:
      return builtin_db_put(args, nargs);
    case B_DB_GET:
      /* The value-level read, not the byte layer's. This id is shared with
       * `builtin_db_get` above, and dispatching it to the byte reader is exactly
       * the bug the value layer exists to fix: `db-set` would store JSON and
       * `db-get` would hand back the raw text, so a set/get pair would not
       * round-trip its own argument in a compiled binary while it did in the
       * interpreter. The byte reader is kept, and reachable, as
       * `builtin_db_get_raw` — see dbkv.rs. */
      return builtin_db_get_kv(args, nargs);
    case B_DB_FLUSH:
      return builtin_db_flush(args, nargs);
    case B_DB_CLOSE:
      return builtin_db_close(args, nargs);
    /* Tier 4 key-value layer */
    case B_DB_SET:
      return builtin_db_set(args, nargs);
    case B_DB_GET_RAW:
      return builtin_db_get_raw(args, nargs);
    case B_DB_DEL:
      return builtin_db_del(args, nargs);
    case B_DB_KEYS:
      return builtin_db_keys(args, nargs);
    case B_DB_COUNT:
      return builtin_db_count(args, nargs);
    /* Tier 4 table layer */
    case B_DB_CREATE_TABLE:
      return builtin_db_create_table(args, nargs);
    case B_DB_INSERT:
      return builtin_db_insert(args, nargs);
    case B_DB_SELECT:
      return builtin_db_select(args, nargs);
    case B_DB_DELETE_ROW:
      return builtin_db_delete_row(args, nargs);
    case B_DB_ALL_ROWS:
      return builtin_db_all_rows(args, nargs);
    case B_DB_QUERY:
      return builtin_db_query(args, nargs);
    case B_DB_QUERY_COUNT:
      return builtin_db_query_count(args, nargs);
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
  return v.tag == V_INT || v.tag == V_BIGINT || v.tag == V_FLOAT;
}
static inline int a_is_int(Value v) {
  return v.tag == V_INT || v.tag == V_BIGINT;
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
      return v_int(r); /* fast path: no allocation, no overflow */
    BigNum *r2 = bignum_add(bignum_i64_to_big(a.u.i), bignum_i64_to_big(b.u.i));
    return v_from_bignum(r2);
  }
  if (a_is_int(a) && a_is_int(b)) {
    BigNum *ta = (a.tag == V_INT) ? bignum_i64_to_big(a.u.i)
                                  : bignum_clone_ref(a.u.bn);
    BigNum *tb = (b.tag == V_INT) ? bignum_i64_to_big(b.u.i)
                                  : bignum_clone_ref(b.u.bn);
    BigNum *r = bignum_add(ta, tb);
    bignum_unref(ta);
    bignum_unref(tb);
    return v_from_bignum(r);
  }
  if (!a_is_num(a) || !a_is_num(b)) {
    a_num_fail(a, b);
    return v_nil();
  }
  double x = as_f64(&a);
  if (g_err)
    return v_nil();
  double y = as_f64(&b);
  if (g_err)
    return v_nil();
  return v_float(x + y);
}
static inline Value a_sub2(Value a, Value b) {
  if (a.tag == V_INT && b.tag == V_INT) {
    int64_t r;
    if (checked_sub(a.u.i, b.u.i, &r))
      return v_int(r); /* fast path: no allocation, no overflow */
    BigNum *r2 = bignum_sub(bignum_i64_to_big(a.u.i), bignum_i64_to_big(b.u.i));
    return v_from_bignum(r2);
  }
  if (a_is_int(a) && a_is_int(b)) {
    BigNum *ta = (a.tag == V_INT) ? bignum_i64_to_big(a.u.i)
                                  : bignum_clone_ref(a.u.bn);
    BigNum *tb = (b.tag == V_INT) ? bignum_i64_to_big(b.u.i)
                                  : bignum_clone_ref(b.u.bn);
    BigNum *r = bignum_sub(ta, tb);
    bignum_unref(ta);
    bignum_unref(tb);
    return v_from_bignum(r);
  }
  if (!a_is_num(a) || !a_is_num(b)) {
    a_num_fail(a, b);
    return v_nil();
  }
  double x = as_f64(&a);
  if (g_err)
    return v_nil();
  double y = as_f64(&b);
  if (g_err)
    return v_nil();
  return v_float(x - y);
}
static inline Value a_mul(Value a, Value b) {
  if (a.tag == V_INT && b.tag == V_INT) {
    int64_t r;
    if (checked_mul(a.u.i, b.u.i, &r))
      return v_int(r); /* fast path: no allocation, no overflow */
    BigNum *r2 = bignum_mul(bignum_i64_to_big(a.u.i), bignum_i64_to_big(b.u.i));
    return v_from_bignum(r2);
  }
  if (a_is_int(a) && a_is_int(b)) {
    BigNum *ta = (a.tag == V_INT) ? bignum_i64_to_big(a.u.i)
                                  : bignum_clone_ref(a.u.bn);
    BigNum *tb = (b.tag == V_INT) ? bignum_i64_to_big(b.u.i)
                                  : bignum_clone_ref(b.u.bn);
    BigNum *r = bignum_mul(ta, tb);
    bignum_unref(ta);
    bignum_unref(tb);
    return v_from_bignum(r);
  }
  if (!a_is_num(a) || !a_is_num(b)) {
    a_num_fail(a, b);
    return v_nil();
  }
  double x = as_f64(&a);
  if (g_err)
    return v_nil();
  double y = as_f64(&b);
  if (g_err)
    return v_nil();
  return v_float(x * y);
}
static inline Value a_div(Value a, Value b) {
  if (!a_is_num(a) || !a_is_num(b)) {
    a_num_fail(a, b);
    return v_nil();
  }
  double x = as_f64(&a);
  if (g_err)
    return v_nil();
  double y = as_f64(&b);
  if (g_err)
    return v_nil();
  if (y == 0.0) {
    set_err("division by zero");
    return v_nil();
  }
  return v_float(x / y);
}
/* Exact numeric ordering for the a_* comparison hot paths: int-vs-int (incl.
 * V_BIGINT) is compared exactly, never through f64; anything with a float goes
 * through f64 (mirrors the interpreter's compare()). op: 0 <, 1 >, 2 <=, 3 >=.
 * Returns 1/0, or -1 on error (NaN / non-number). */
static inline int a_cmp_op(Value a, Value b, int op) {
  if (a_is_int(a) && a_is_int(b)) {
    int c = int_order(&a, &b);
    switch (op) {
    case 0:
      return c < 0;
    case 1:
      return c > 0;
    case 2:
      return c <= 0;
    default:
      return c >= 0;
    }
  }
  if (!a_is_num(a) || !a_is_num(b)) {
    a_num_fail(a, b);
    return -1;
  }
  double x = as_f64(&a);
  if (g_err)
    return -1;
  double y = as_f64(&b);
  if (g_err)
    return -1;
  if (isnan(x) || isnan(y)) {
    set_err("cannot compare NaN");
    return -1;
  }
  switch (op) {
  case 0:
    return x < y;
  case 1:
    return x > y;
  case 2:
    return x <= y;
  default:
    return x >= y;
  }
}
static inline Value a_lt(Value a, Value b) {
  int r = a_cmp_op(a, b, 0);
  return r < 0 ? v_nil() : v_bool(r);
}
static inline Value a_gt(Value a, Value b) {
  int r = a_cmp_op(a, b, 1);
  return r < 0 ? v_nil() : v_bool(r);
}
static inline Value a_le(Value a, Value b) {
  int r = a_cmp_op(a, b, 2);
  return r < 0 ? v_nil() : v_bool(r);
}
static inline Value a_ge(Value a, Value b) {
  int r = a_cmp_op(a, b, 3);
  return r < 0 ? v_nil() : v_bool(r);
}
static inline Value a_eq2(Value a, Value b) { return v_bool(values_eq(&a, &b)); }
static inline Value a_mod(Value a, Value b) {
  if (!a_is_int(a) || !a_is_int(b)) {
    set_err("mod expects (mod int int)");
    return v_nil();
  }
  BigNum *ta = (a.tag == V_INT) ? bignum_i64_to_big(a.u.i)
                                : bignum_clone_ref(a.u.bn);
  BigNum *tb = (b.tag == V_INT) ? bignum_i64_to_big(b.u.i)
                                : bignum_clone_ref(b.u.bn);
  if (bignum_is_zero(tb)) {
    set_err("mod by zero");
    bignum_unref(ta);
    bignum_unref(tb);
    return v_nil();
  }
  BigNum *r = bignum_mod(ta, tb);
  bignum_unref(ta);
  bignum_unref(tb);
  return v_from_bignum(r);
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

/* ---- JSON --------------------------------------------------------------- */
/* The normative implementation is crates/ainl-core/src/json_value.rs; this is
 * a line-for-line port of it, including every error message. The four design
 * decisions (string keys only, insertion-order objects, one canonical float
 * spelling, non-finite floats are an error) are documented there and each is
 * cited below where it is implemented.
 *
 * The parser walks raw bytes rather than UTF-8 code points so its error
 * offsets are byte offsets, exactly like the Rust parser's `self.i`. */
#define JSON_MAX_DEPTH 512

typedef struct {
  const char *b;
  size_t len;
  size_t i;
} JParser;

static Value jp_err(JParser *p, const char *msg) {
  set_err("json-parse: %s at position %zu", msg, p->i);
  return v_nil();
}
static void jp_ws(JParser *p) {
  while (p->i < p->len) {
    char c = p->b[p->i];
    if (c == ' ' || c == '\t' || c == '\n' || c == '\r')
      p->i++;
    else
      break;
  }
}
static int jp_hex4(JParser *p, unsigned int *out) {
  if (p->i + 4 > p->len) {
    set_err("json-parse: truncated \\u escape at position %zu", p->i);
    return 0;
  }
  unsigned int v = 0;
  for (int k = 0; k < 4; k++) {
    char c = p->b[p->i + k];
    unsigned int d;
    if (c >= '0' && c <= '9')
      d = (unsigned int)(c - '0');
    else if (c >= 'a' && c <= 'f')
      d = (unsigned int)(c - 'a') + 10;
    else if (c >= 'A' && c <= 'F')
      d = (unsigned int)(c - 'A') + 10;
    else {
      set_err("json-parse: invalid \\u escape at position %zu", p->i);
      return 0;
    }
    v = v * 16 + d;
  }
  p->i += 4;
  *out = v;
  return 1;
}
/* Decode a code point to UTF-8 into `buf` (max 4 bytes); returns its length.
 * Lone surrogates and out-of-range values are errors in the caller. */
static size_t utf8_encode(unsigned int cp, char *buf) {
  if (cp < 0x80) {
    buf[0] = (char)cp;
    return 1;
  }
  if (cp < 0x800) {
    buf[0] = (char)(0xC0 | (cp >> 6));
    buf[1] = (char)(0x80 | (cp & 0x3F));
    return 2;
  }
  if (cp < 0x10000) {
    buf[0] = (char)(0xE0 | (cp >> 12));
    buf[1] = (char)(0x80 | ((cp >> 6) & 0x3F));
    buf[2] = (char)(0x80 | (cp & 0x3F));
    return 3;
  }
  buf[0] = (char)(0xF0 | (cp >> 18));
  buf[1] = (char)(0x80 | ((cp >> 12) & 0x3F));
  buf[2] = (char)(0x80 | ((cp >> 6) & 0x3F));
  buf[3] = (char)(0x80 | (cp & 0x3F));
  return 4;
}
/* Read a JSON string body (the opening quote is already consumed). On success
 * returns a malloc'd NUL-terminated UTF-8 buffer and sets *out_len. */
static char *jp_string(JParser *p, size_t *out_len) {
  size_t cap = 32, w = 0;
  char *out = malloc(cap);
  if (!out) {
    set_err("json-parse: out of memory");
    return NULL;
  }
  for (;;) {
    if (p->i >= p->len) {
      free(out);
      set_err("json-parse: unterminated string at position %zu", p->i);
      return NULL;
    }
    unsigned char c = (unsigned char)p->b[p->i];
    if (c == '"') {
      p->i++;
      out[w] = 0;
      *out_len = w;
      return out;
    }
    /* Room for one more UTF-8 sequence (4 bytes) plus the terminator. */
    if (w + 5 > cap) {
      size_t ncap = cap * 2;
      char *n = realloc(out, ncap);
      if (!n) {
        free(out);
        set_err("json-parse: out of memory");
        return NULL;
      }
      out = n;
      cap = ncap;
    }
    p->i++;
    if (c == '\\') {
      if (p->i >= p->len) {
        free(out);
        set_err("json-parse: unterminated escape at position %zu", p->i);
        return NULL;
      }
      char e = p->b[p->i++];
      /* \b and \f are read (JSON allows both) but never written — see
       * json_write_string in the Rust module for why one spelling wins. */
      switch (e) {
      case '"': out[w++] = '"'; break;
      case '\\': out[w++] = '\\'; break;
      case '/': out[w++] = '/'; break;
      case 'b': out[w++] = '\b'; break;
      case 'f': out[w++] = '\f'; break;
      case 'n': out[w++] = '\n'; break;
      case 'r': out[w++] = '\r'; break;
      case 't': out[w++] = '\t'; break;
      case 'u': {
        unsigned int hi;
        if (!jp_hex4(p, &hi)) {
          free(out);
          return NULL;
        }
        unsigned int cp;
        if (hi >= 0xD800 && hi <= 0xDBFF) {
          /* High surrogate: must be followed by \uDC00..\uDFFF. */
          if (!(p->i + 1 < p->len && p->b[p->i] == '\\' && p->b[p->i + 1] == 'u')) {
            free(out);
            jp_err(p, "unpaired surrogate");
            return NULL;
          }
          p->i += 2;
          unsigned int lo;
          if (!jp_hex4(p, &lo)) {
            free(out);
            return NULL;
          }
          if (lo < 0xDC00 || lo > 0xDFFF) {
            free(out);
            jp_err(p, "invalid low surrogate");
            return NULL;
          }
          cp = 0x10000u + ((hi - 0xD800u) << 10) + (lo - 0xDC00u);
        } else if (hi >= 0xDC00 && hi <= 0xDFFF) {
          free(out);
          jp_err(p, "unpaired surrogate");
          return NULL;
        } else {
          cp = hi;
        }
        w += utf8_encode(cp, out + w);
        break;
      }
      default:
        free(out);
        jp_err(p, "invalid escape");
        return NULL;
      } /* end switch (e) */
    } else if (c < 0x20) {
      /* A raw control character is not legal inside a JSON string. Rejecting
       * it is what makes a serialize/parse round-trip always produce a
       * document a parser accepts. */
      free(out);
      jp_err(p, "control character in string");
      return NULL;
    } else if (c < 0x80) {
      out[w++] = (char)c;
    } else {
      /* Literal UTF-8: copy the whole sequence. AINL strings are validated
       * UTF-8 at creation, so a lead byte here always starts a character. */
      size_t start = p->i - 1;
      size_t end = p->i;
      while (end < p->len && ((unsigned char)p->b[end] & 0xC0) == 0x80)
        end++;
      size_t n = end - start;
      while (w + n + 1 > cap) {
        size_t ncap = cap * 2;
        char *nn = realloc(out, ncap);
        if (!nn) {
          free(out);
          set_err("json-parse: out of memory");
          return NULL;
        }
        out = nn;
        cap = ncap;
      }
      memcpy(out + w, p->b + start, n);
      w += n;
      p->i = end;
    }
  }
}
static Value jp_value(JParser *p, int depth);

static Value jp_object(JParser *p, int depth) {
  p->i++; /* '{' */
  Map *m = malloc(sizeof(Map));
  m->ref = 1;
  m->n = 0;
  m->keys = malloc(sizeof(Value));
  m->vals = malloc(sizeof(Value));
  jp_ws(p);
  if (p->i < p->len && p->b[p->i] == '}') {
    p->i++;
    Value r;
    r.tag = V_MAP;
    r.u.m = m;
    return r;
  }
  for (;;) {
    jp_ws(p);
    if (p->i >= p->len || p->b[p->i] != '"') {
      map_free(m);
      return jp_err(p, "expected a string key");
    }
    p->i++;
    size_t klen;
    char *kdata = jp_string(p, &klen);
    if (!kdata) {
      map_free(m);
      return v_nil();
    }
    Str *kst = malloc(sizeof(Str));
    kst->ref = 1;
    kst->len = klen;
    kst->data = kdata;
    Value kv;
    kv.tag = V_STR;
    kv.u.s = kst;
    jp_ws(p);
    if (p->i >= p->len || p->b[p->i] != ':') {
      v_unref(&kv);
      map_free(m);
      return jp_err(p, "expected ':' after a key");
    }
    p->i++;
    Value v = jp_value(p, depth + 1);
    if (g_err) {
      v_unref(&kv);
      map_free(m);
      return v_nil();
    }
    /* Same "last value wins, first position" rule as hash/assoc. */
    int found = -1;
    for (int i = 0; i < m->n; i++) {
      if (values_eq(&m->keys[i], &kv)) {
        found = i;
        break;
      }
    }
    if (found >= 0) {
      v_unref(&m->vals[found]);
      m->vals[found] = v;
      v_unref(&kv);
    } else {
      m->keys = realloc(m->keys, (size_t)(m->n + 1) * sizeof(Value));
      m->vals = realloc(m->vals, (size_t)(m->n + 1) * sizeof(Value));
      m->keys[m->n] = kv;
      m->vals[m->n] = v;
      m->n++;
    }
    jp_ws(p);
    if (p->i < p->len && p->b[p->i] == ',') {
      p->i++;
      continue;
    }
    if (p->i < p->len && p->b[p->i] == '}') {
      p->i++;
      Value r;
      r.tag = V_MAP;
      r.u.m = m;
      return r;
    }
    map_free(m);
    return jp_err(p, "expected ',' or '}'");
  }
}
static Value jp_array(JParser *p, int depth) {
  p->i++; /* '[' */
  Value *items = NULL;
  int n = 0, cap = 0;
  jp_ws(p);
  if (p->i < p->len && p->b[p->i] == ']') {
    p->i++;
    return v_list_empty();
  }
  for (;;) {
    if (n == cap) {
      cap = cap ? cap * 2 : 8;
      Value *ni = realloc(items, (size_t)cap * sizeof(Value));
      if (!ni) {
        for (int i = 0; i < n; i++)
          v_unref(&items[i]);
        free(items);
        set_err("json-parse: out of memory");
        return v_nil();
      }
      items = ni;
    }
    Value v = jp_value(p, depth + 1);
    if (g_err) {
      for (int i = 0; i < n; i++)
        v_unref(&items[i]);
      free(items);
      return v_nil();
    }
    items[n++] = v;
    jp_ws(p);
    if (p->i < p->len && p->b[p->i] == ',') {
      p->i++;
      continue;
    }
    if (p->i < p->len && p->b[p->i] == ']') {
      p->i++;
      return v_list_from_array(items, n);
    }
    for (int i = 0; i < n; i++)
      v_unref(&items[i]);
    free(items);
    return jp_err(p, "expected ',' or ']'");
  }
}
static Value jp_number(JParser *p) {
  size_t start = p->i;
  if (p->i < p->len && p->b[p->i] == '-')
    p->i++;
  if (p->i >= p->len) {
    set_err("json-parse: expected a digit at position %zu", p->i);
    return v_nil();
  }
  if (p->b[p->i] == '0') {
    p->i++;
    if (p->i < p->len && p->b[p->i] >= '0' && p->b[p->i] <= '9') {
      set_err("json-parse: leading zero in number at position %zu", p->i);
      return v_nil();
    }
  } else if (p->b[p->i] >= '1' && p->b[p->i] <= '9') {
    while (p->i < p->len && p->b[p->i] >= '0' && p->b[p->i] <= '9')
      p->i++;
  } else {
    set_err("json-parse: expected a digit at position %zu", p->i);
    return v_nil();
  }
  int is_float = 0;
  if (p->i < p->len && p->b[p->i] == '.') {
    is_float = 1;
    p->i++;
    if (!(p->i < p->len && p->b[p->i] >= '0' && p->b[p->i] <= '9')) {
      set_err("json-parse: expected a digit after '.' at position %zu", p->i);
      return v_nil();
    }
    while (p->i < p->len && p->b[p->i] >= '0' && p->b[p->i] <= '9')
      p->i++;
  }
  if (p->i < p->len && (p->b[p->i] == 'e' || p->b[p->i] == 'E')) {
    is_float = 1;
    p->i++;
    if (p->i < p->len && (p->b[p->i] == '+' || p->b[p->i] == '-'))
      p->i++;
    if (!(p->i < p->len && p->b[p->i] >= '0' && p->b[p->i] <= '9')) {
      set_err("json-parse: expected a digit in the exponent at position %zu", p->i);
      return v_nil();
    }
    while (p->i < p->len && p->b[p->i] >= '0' && p->b[p->i] <= '9')
      p->i++;
  }
  size_t n = p->i - start;
  char *text = malloc(n + 1);
  memcpy(text, p->b + start, n);
  text[n] = 0;
  if (!is_float) {
    /* An integer literal that fits i64 becomes Value::Int, so `(get (json-parse
     * "{\"a\":1}") "a")` is an int and prints `1`. Anything larger becomes an
     * f64 — the same i64-range rule the rest of AINL uses, and what the Rust
     * parser's `text.parse::<i64>()` does.
     *
     * Overflow is detected by digit count rather than by inspecting strtoll's
     * return: strtoll *saturates* on overflow (returning LLONG_MAX/MIN with
     * errno set), so "did it parse everything" and "is it in range" would be
     * two different checks on a value that already lost information. Counting
     * digits answers the range question directly. */
    const char *d = text;
    int neg = (*d == '-');
    if (neg)
      d++;
    size_t ndigits = strlen(d);
    /* i64: at most 19 digits, or exactly 19 starting with '1'..'8' (or '9' with
     * no more digits, for INT64_MAX). INT64_MIN has 19 digits starting '9'. */
    int fits = 0;
    if (ndigits <= 18) {
      fits = 1;
    } else if (ndigits == 19) {
      /* 19 digits: compare against the 19-digit bounds, ignoring the sign for
       * the magnitude (the magnitude bound is 2^63 for the negative side). */
      static const char *max19 = "9223372036854775807";
      static const char *minmag19 = "9223372036854775808";
      fits = neg ? (strcmp(d, minmag19) <= 0) : (strcmp(d, max19) <= 0);
    }
    if (fits) {
      char *endp = NULL;
      long long ll = strtoll(text, &endp, 10);
      if (endp && *endp == 0) {
        free(text);
        return v_int((int64_t)ll);
      }
    }
  }
  double dv = strtod(text, NULL);
  free(text);
  return v_float(dv);
}
static Value jp_value(JParser *p, int depth) {
  if (depth > JSON_MAX_DEPTH) {
    set_err("json-parse: nesting too deep (max %d levels)", JSON_MAX_DEPTH);
    return v_nil();
  }
  jp_ws(p);
  if (p->i >= p->len) {
    set_err("json-parse: unexpected end of input at position %zu", p->i);
    return v_nil();
  }
  char c = p->b[p->i];
  if (c == '{')
    return jp_object(p, depth);
  if (c == '[')
    return jp_array(p, depth);
  if (c == '"') {
    p->i++;
    size_t slen;
    char *s = jp_string(p, &slen);
    if (!s)
      return v_nil();
    Str *st = malloc(sizeof(Str));
    st->ref = 1;
    st->len = slen;
    st->data = s;
    Value r;
    r.tag = V_STR;
    r.u.s = st;
    return r;
  }
  if (c == 't') {
    if (p->i + 4 <= p->len && memcmp(p->b + p->i, "true", 4) == 0) {
      p->i += 4;
      return v_bool(1);
    }
    return jp_err(p, "expected 'true'");
  }
  if (c == 'f') {
    if (p->i + 5 <= p->len && memcmp(p->b + p->i, "false", 5) == 0) {
      p->i += 5;
      return v_bool(0);
    }
    return jp_err(p, "expected 'false'");
  }
  if (c == 'n') {
    if (p->i + 4 <= p->len && memcmp(p->b + p->i, "null", 4) == 0) {
      p->i += 4;
      return v_nil();
    }
    return jp_err(p, "expected 'null'");
  }
  if (c == '-' || (c >= '0' && c <= '9'))
    return jp_number(p);
  return jp_err(p, "unexpected character");
}

/* (json-parse string) -> value */
static Value builtin_json_parse(Value *args, int nargs) {
  if (nargs != 1) {
    set_err("json-parse expects (json-parse string)");
    return v_nil();
  }
  const char *s = as_str_arg(&args[0], "json-parse");
  if (!s)
    return v_nil();
  JParser p;
  p.b = s;
  p.len = args[0].u.s->len; /* byte length, not strlen: a NUL is possible */
  p.i = 0;
  Value v = jp_value(&p, 0);
  if (g_err)
    return v_nil();
  jp_ws(&p);
  if (p.i != p.len) {
    return jp_err(&p, "trailing content after the value");
  }
  return v;
}

/* ---- json-serialize --------------------------------------------------- */
/* A growable byte buffer. json-serialize output can be much larger than its
 * input (1e300 is a 303-byte literal), so nothing here can be a fixed size. */
typedef struct {
  char *p;
  size_t len;
  size_t cap;
} Buf;
static int buf_reserve(Buf *b, size_t extra) {
  if (b->len + extra + 1 <= b->cap)
    return 1;
  size_t ncap = b->cap ? b->cap : 128;
  while (ncap < b->len + extra + 1)
    ncap *= 2;
  char *np = realloc(b->p, ncap);
  if (!np)
    return 0;
  b->p = np;
  b->cap = ncap;
  return 1;
}
static int buf_put(Buf *b, const char *s, size_t n) {
  if (!buf_reserve(b, n))
    return 0;
  memcpy(b->p + b->len, s, n);
  b->len += n;
  b->p[b->len] = 0;
  return 1;
}
static int buf_putc(Buf *b, char c) { return buf_put(b, &c, 1); }

/* Write a JSON string literal. One spelling per escape, matching the Rust
 * module: `"` `\\` `\n` `\r` `\t`, and `\u00xx` for every other C0 control —
 * notably NOT `\b` or `\f`, which the reader accepts but the writer never
 * emits, so all four backends produce identical bytes. Non-ASCII is emitted as
 * literal UTF-8. */
static int json_write_string(Buf *b, Str *s) {
  if (!buf_putc(b, '"'))
    return 0;
  for (size_t i = 0; i < s->len; i++) {
    unsigned char c = (unsigned char)s->data[i];
    switch (c) {
    case '"': if (!buf_put(b, "\\\"", 2)) return 0; break;
    case '\\': if (!buf_put(b, "\\\\", 2)) return 0; break;
    case '\n': if (!buf_put(b, "\\n", 2)) return 0; break;
    case '\r': if (!buf_put(b, "\\r", 2)) return 0; break;
    case '\t': if (!buf_put(b, "\\t", 2)) return 0; break;
    default:
      if (c < 0x20) {
        char esc[7];
        snprintf(esc, sizeof(esc), "\\u%04x", c);
        if (!buf_put(b, esc, 6))
          return 0;
      } else {
        /* Copy the whole UTF-8 sequence verbatim. */
        size_t n = 1;
        if (c >= 0xF0)
          n = 4;
        else if (c >= 0xE0)
          n = 3;
        else if (c >= 0xC0)
          n = 2;
        if (i + n > s->len)
          n = 1;
        if (!buf_put(b, s->data + i, n))
          return 0;
        i += n - 1;
      }
    }
  }
  return buf_putc(b, '"');
}

/* Decision 3: the one canonical float spelling — the *shortest* decimal that
 * round-trips, in fixed-point notation, with a mandatory ".0" on a whole
 * value.
 *
 * NOT format_float() above: that is a port of `Value`'s Display, which prints
 * a whole float's exact binary expansion (303 characters for 1e300) rather
 * than the shortest form. The shortest form is what all four backends can
 * actually compute — JS's toFixed is undefined above 1e21 and returns
 * exponential form there — so it is the rule. The digits come from the same
 * %.{p}e shortening loop format_float uses, but the trailing ".0" is appended
 * to the shortest form rather than expanded exactly. */
static int json_write_float(Buf *b, double x) {
  if (isnan(x)) {
    set_err("json-serialize: cannot serialize NaN (not a finite number)");
    return 0;
  }
  if (x == INFINITY || x == -INFINITY) {
    set_err("json-serialize: cannot serialize %s (not a finite number)",
            x == INFINITY ? "inf" : "-inf");
    return 0;
  }
  if (x == 0.0)
    return buf_put(b, "0.0", 3); /* also normalizes -0.0 */
  int neg = signbit(x);
  double ax = neg ? -x : x;
  /* Shortest round-tripping significant digits. */
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
  /* tmp is "d[.ddd]e±NN" — split it into digits and a decimal-point position. */
  char *e = strchr(tmp, 'e');
  int exp10 = (int)strtol(e + 1, NULL, 10);
  char digits[32];
  int nd = 0;
  for (char *q = tmp; q < e; q++) {
    if (*q != '.')
      digits[nd++] = *q;
  }
  digits[nd] = 0;
  /* value = 0.digits * 10^point */
  int point = exp10 + 1;
  char out[4096];
  int oi = 0;
  if (point <= 0) {
    out[oi++] = '0';
    out[oi++] = '.';
    for (int k = 0; k < -point; k++)
      out[oi++] = '0';
    for (int k = 0; k < nd; k++)
      out[oi++] = digits[k];
  } else if (point >= nd) {
    for (int k = 0; k < nd; k++)
      out[oi++] = digits[k];
    for (int k = 0; k < point - nd; k++)
      out[oi++] = '0';
    out[oi++] = '.';
    out[oi++] = '0'; /* the ".0" that marks it a float */
  } else {
    for (int k = 0; k < point; k++)
      out[oi++] = digits[k];
    out[oi++] = '.';
    for (int k = point; k < nd; k++)
      out[oi++] = digits[k];
  }
  out[oi] = 0;
  if (neg) {
    memmove(out + 1, out, (size_t)oi);
    out[0] = '-';
    out[oi + 1] = 0;
  }
  return buf_put(b, out, strlen(out));
}

static int json_write_value(Buf *b, Value *v, int depth) {
  if (depth > JSON_MAX_DEPTH) {
    set_err("json-serialize: nesting too deep (max %d levels)", JSON_MAX_DEPTH);
    return 0;
  }
  switch (v->tag) {
  case V_NIL: return buf_put(b, "null", 4);
  case V_BOOL: return buf_put(b, v->u.b ? "true" : "false", v->u.b ? 4 : 5);
  case V_INT: {
    char tmp[32];
    snprintf(tmp, sizeof(tmp), "%lld", (long long)v->u.i);
    return buf_put(b, tmp, strlen(tmp));
  }
  case V_FLOAT: return json_write_float(b, v->u.f);
  case V_STR: return json_write_string(b, v->u.s);
  case V_LIST: {
    if (!buf_putc(b, '['))
      return 0;
    ConsCell *c = v->u.l;
    int first = 1;
    while (c && c->len > 0) {
      if (!first && !buf_putc(b, ','))
        return 0;
      first = 0;
      if (!json_write_value(b, &c->head, depth + 1))
        return 0;
      c = c->tail;
    }
    return buf_putc(b, ']');
  }
  case V_MAP: {
    if (!buf_putc(b, '{'))
      return 0;
    Map *m = v->u.m;
    for (int i = 0; i < m->n; i++) {
      if (i > 0 && !buf_putc(b, ','))
        return 0;
      /* Decision 1: a non-string key has no faithful JSON form. */
      if (m->keys[i].tag != V_STR) {
        set_err("json-serialize: object keys must be str, got %s",
                type_name(&m->keys[i]));
        return 0;
      }
      if (!json_write_string(b, m->keys[i].u.s))
        return 0;
      if (!buf_putc(b, ':'))
        return 0;
      if (!json_write_value(b, &m->vals[i], depth + 1))
        return 0;
    }
    return buf_putc(b, '}');
  }
  case V_SYM:
    set_err("json-serialize: cannot serialize a sym");
    return 0;
  default:
    set_err("json-serialize: cannot serialize a %s", type_name(v));
    return 0;
  }
}

/* (json-serialize value) -> str */
static Value builtin_json_serialize(Value *args, int nargs) {
  if (nargs != 1) {
    set_err("json-serialize expects (json-serialize value)");
    return v_nil();
  }
  Buf b;
  b.p = NULL;
  b.len = 0;
  b.cap = 0;
  if (!json_write_value(&b, &args[0], 0)) {
    free(b.p);
    if (!g_err)
      set_err("json-serialize: out of memory");
    return v_nil();
  }
  if (!b.p) {
    /* An empty output cannot happen (every value writes at least one byte),
     * but v_str_take would take a NULL. */
    b.p = malloc(1);
    b.p[0] = 0;
  }
  return v_str_take(b.p);
}

/* ---- testing (Tier 2) -------------------------------------------------- */
/* (test name expr expected) -> true, or a runtime error naming the test, the
 * expected value and the actual value.
 *
 * A failure raises rather than printing, so the message is one string the
 * 4-backend rule can hold byte-for-byte: the same shared body every other
 * error builtin produces. Like every other AOT diagnostic it carries no source
 * position — the standalone binary does not embed the source, so there is no
 * line/column to print. See docs/SYNTAX.md 5a. */
static Value builtin_test(Value *args, int nargs) {
  if (nargs != 3) {
    set_err("test expects (test name expr expected)");
    return v_nil();
  }
  if (args[0].tag != V_STR) {
    set_err("test expects a str name, got %s", type_name(&args[0]));
    return v_nil();
  }
  if (args[2].tag != V_STR) {
    set_err("test expects a str expected value, got %s", type_name(&args[2]));
    return v_nil();
  }
  char actual[8192];
  value_to_string(&args[1], actual, sizeof(actual));
  if (strcmp(args[2].u.s->data, actual) == 0)
    return v_bool(1);
  set_err("test failed: %s: expected %s, got %s", args[0].u.s->data,
          args[2].u.s->data, actual);
  return v_nil();
}

/* ---- sort (Tier 3 collections) ------------------------------------------ */

/* The default ordering: numbers by value, strings bytewise, nothing else.
 *
 * A mixed-type list is REJECTED rather than given a defined-but-arbitrary
 * order. A sort that quietly put every number before every string would return a
 * stable, reproducible answer to a program that has a bug in it, and that bug
 * would surface much later as a wrong number instead of here as a type error.
 *
 * int and float compare by value, not by tag: the runtime's own value equality
 * already mixes them ((= 1 1.0) is true), and a sort that ordered [1, 1.0] by
 * tag would answer a different question than the language does. This mirrors
 * `ainl_core::collections::default_compare`, and every error string here is
 * pinned to that module's wording so the two backends stay byte-identical. */
static int sort_default_cmp(Value *a, Value *b) {
  int a_num = (a->tag == V_INT || a->tag == V_FLOAT);
  int b_num = (b->tag == V_INT || b->tag == V_FLOAT);
  if (a_num && b_num) {
    double x = as_f64(a), y = as_f64(b);
    if (x < y)
      return -1;
    if (x > y)
      return 1;
    return 0;
  }
  if (a->tag == V_STR && b->tag == V_STR) {
    /* Bytewise, deliberately. Python compares code points, JavaScript compares
     * UTF-16 code units, Ruby compares bytes; only "bytes" is the same total
     * order in all three, and this runtime spells it strcmp. */
    int r = strcmp(a->u.s->data, b->u.s->data);
    return r < 0 ? -1 : (r > 0 ? 1 : 0);
  }
  set_err("sort expects a list of numbers or of strings, got a list mixing %s and %s",
          type_name(a), type_name(b));
  return 0;
}

/* Normalise a comparator's result to -1/0/1.
 *
 * A comparator that returned a bool or a string is a mistake worth naming:
 * coercing it to 0 would make a broken comparator look like an "equal" one and
 * leave the list in input order, which reads as a working sort. */
static int sort_comparator_sign(Value *res) {
  double d;
  if (res->tag != V_INT && res->tag != V_FLOAT) {
    set_err("sort comparator must return a number, got %s", type_name(res));
    return 0;
  }
  d = as_f64(res);
  if (d < 0.0)
    return -1;
  if (d > 0.0)
    return 1;
  return 0;
}

/* Bottom-up stable merge sort over an array of already-reffed values.
 *
 * Stable: on a tie (`c <= 0`) take from the LEFT run, which is what makes equal
 * elements keep their input order. Iterative rather than recursive so a long
 * list cannot grow the C stack — the same concern that made the cons-cell
 * release iterative. Mirrors `ainl_core::collections::merge_sort`; a host
 * `qsort` is not an option because it is not stable. */
static void sort_merge(Value *items, int n, Value *tmp, Value *cmpfn) {
  int width;
  for (width = 1; width < n; width *= 2) {
    int start;
    for (start = 0; start < n; start += 2 * width) {
      int mid = start + width < n ? start + width : n;
      int end = start + 2 * width < n ? start + 2 * width : n;
      int l = start, r = mid, k = start;
      while (l < mid && r < end) {
        int c;
        if (cmpfn) {
          Value pair[2];
          Value res;
          pair[0] = items[l];
          pair[1] = items[r];
          res = v_call(*cmpfn, pair, 2);
          if (g_err)
            return;
          c = sort_comparator_sign(&res);
          if (g_err)
            return;
        } else {
          c = sort_default_cmp(&items[l], &items[r]);
          if (g_err)
            return;
        }
        if (c <= 0) {
          tmp[k++] = items[l++];
        } else {
          tmp[k++] = items[r++];
        }
      }
      while (l < mid)
        tmp[k++] = items[l++];
      while (r < end)
        tmp[k++] = items[r++];
    }
    for (start = 0; start < n; start++)
      items[start] = tmp[start];
  }
}

static Value builtin_sort(Value *args, int nargs) {
  Value *cmpfn = NULL;
  Value *listp;
  int n, i;
  Value *items, *tmp, out;

  if (nargs == 2) {
    if (args[0].tag != V_CLOSURE) {
      set_err("sort expects a fn, got %s", type_name(&args[0]));
      return v_nil();
    }
    cmpfn = &args[0];
    listp = &args[1];
  } else if (nargs == 1) {
    listp = &args[0];
  } else {
    set_err("sort expects (sort list) or (sort fn list)");
    return v_nil();
  }
  if (listp->tag != V_LIST) {
    set_err("sort expects a list, got %s", type_name(listp));
    return v_nil();
  }

  /* Flatten to an array so the merge can index it, then build a NEW list. The
   * input is shared, immutable cons cells, so purity is free: nothing here
   * writes through listp. The result is never the input itself — even a
   * 0- or 1-element list comes back as a fresh list. */
  n = listp->u.l->len;
  items = (Value *)malloc(sizeof(Value) * (size_t)(n > 0 ? n : 1));
  if (!items) {
    set_err("out of memory");
    return v_nil();
  }
  {
    ConsCell *cur = listp->u.l;
    for (i = 0; i < n; i++) {
      items[i] = cur->head;
      v_ref(&items[i]);
      cur = cur->tail;
    }
  }

  if (n > 1) {
    tmp = (Value *)malloc(sizeof(Value) * (size_t)n);
    if (!tmp) {
      for (i = 0; i < n; i++)
        v_unref(&items[i]);
      free(items);
      set_err("out of memory");
      return v_nil();
    }
    sort_merge(items, n, tmp, cmpfn);
    free(tmp);
  }
  if (g_err) {
    for (i = 0; i < n; i++)
      v_unref(&items[i]);
    free(items);
    return v_nil();
  }

  out = v_list_from_array(items, n);
  /* No `v_unref` loop here. `v_list_from_array` -> `cons_cell_new` stores each
   * head **without** taking its own reference — the caller passes ownership of
   * the refs it took in, which is why `builtin_list` and `builtin_push` both
   * `free(items)` without unref'ing. Unreffing here therefore drops the list's
   * own reference on every element, and the sorted result comes back holding
   * freed strings: `(sort (list "apple" "fig" "pear"))` printed
   * `("apple" "fig" "")` — the last element's bytes were reclaimed, and which
   * one lost them depended on the merge order, not on anything stable. */
  free(items);
  return out;
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
      /* Tier 1 JSON */
      {"json-parse", B_JSON_PARSE}, {"json-serialize", B_JSON_SERIALIZE},
      /* Tier 2 testing */
      {"test", B_TEST},
      /* Tier 3 collections */
      {"sort", B_SORT},
      /* Byte-oriented string primitives (Tier 3) */
      {"substring", B_SUBSTRING}, {"char", B_CHAR}, {"code", B_CODE},
      {"starts-with", B_STARTS_WITH}, {"ends-with", B_ENDS_WITH},
      {"index-of", B_INDEX_OF},
      /* Tier 3 file system */
      {"mkdir", B_MKDIR}, {"rename", B_RENAME}, {"copy", B_COPY},
      {"is-dir", B_IS_DIR}, {"file-size", B_FILE_SIZE},
      /* Tier 4 storage. `db-get` resolves to B_DB_GET, which the dispatch switch
       * sends to the *value* layer's reader; see the B_DB_GET case in v_call. */
      {"db-open", B_DB_OPEN}, {"db-put", B_DB_PUT}, {"db-get", B_DB_GET},
      {"db-flush", B_DB_FLUSH}, {"db-close", B_DB_CLOSE},
      /* Tier 4 key-value layer */
      {"db-set", B_DB_SET}, {"db-get-raw", B_DB_GET_RAW}, {"db-del", B_DB_DEL},
      {"db-keys", B_DB_KEYS}, {"db-count", B_DB_COUNT},
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
