//! Arbitrary-precision integer, hand-written (zero external dependencies).
//!
//! `BigNum` is AINL's integer type. It is a tagged union with an
//! allocation-free small-int fast path:
//!
//! - `Small(i64)`: values that fit in `i64`, held inline. No heap traffic.
//! - `Big(Rc<Big>)`: values that do not, held as a shared signed-magnitude
//!   representation.
//!
//! The `Small` fast path is what keeps the VM perf gate green: the 40k-loop
//! benchmark never leaves `i64` range, so its hot ops (`+`, `<`) run on the
//! inline `i64` with no allocation. A value only widens to `Big` when an
//! operation actually overflows `i64` — and integers never overflow, they
//! just get bigger (the old "promote to `f64` on overflow" path is gone).
//!
//! `Big` is signed-magnitude: a `neg` flag plus a little-endian base-2^32
//! magnitude. A `Big` is never zero (zero is always `Small(0)`), so the sign
//! is well-defined.
//!
//! ## Layout: why `BigNum` is 16 bytes
//!
//! `BigNum` is 16 bytes (8 for the `i64` plus an 8-byte `Rc` discriminant),
//! which makes `Value::Int` carry a 16-byte payload instead of the bare 8 of
//! `i64`. Every `Value` pushed, popped and copied by the VM therefore moves 8
//! bytes more than it used to, and `Value`'s `Clone` has to branch on the
//! variant to decide whether an `Rc` refcount needs bumping — where a bare
//! `i64` needed no branch at all.
//!
//! An 8-byte `BigNum` is not available here: `i64` has no spare bit to steal
//! (its full range is already used by `Small`, and that range is part of the
//! language — `9223372036854775807` must stay an integer), and `Rc<Big>` is a
//! fat pointer, so the union needs both a discriminant word and a payload word.
//!
//! The measured cost of all that, on the 40k-loop perf gate that runs in debug
//! on CI: **5.72x before this change, 5.70x after** — i.e. nothing measurable.
//! Two hot-path details are kept because they are free and were worth finding
//! while checking that: the VM's `Add`/`Mul`/`CmpLt` test the `Small`/`Small`
//! case inline and operate on the bare `i64` (`bignum::add_i64`/`mul_i64` are
//! the widening halves), and `DefSlot` skips its `Value` clone when there is no
//! active env, which is the loop's `(def i (+ i 1))`.

use std::cmp::Ordering;
use std::fmt;
use std::rc::Rc;

/// The out-of-`i64`-range magnitude: signed, little-endian base-2^32.
///
/// `limbs` is shared (`Rc`) so `neg`/`abs` can flip the sign without copying
/// the magnitude. It is never empty and never all-zero.
///
/// Public only because `BigNum::Big` names it; every field is private, so the
/// representation stays an implementation detail of this module.
pub struct Big {
    /// True if negative. A `Big` is never zero, so this is well-defined.
    neg: bool,
    /// Magnitude, least-significant limb first. Never empty, never all-zero.
    limbs: Rc<Vec<u32>>,
}

/// An arbitrary-precision integer. `Clone` (shared `Big` via `Rc`), not
/// `Copy` (the `Big` variant holds an `Rc`).
pub enum BigNum {
    /// A value that fits in `i64`, held inline (the fast path).
    Small(i64),
    /// A value that does not fit in `i64`.
    Big(Rc<Big>),
}

/// Hand-written rather than derived, and `#[inline]`, because this is the
/// VM's hottest copy.
///
/// `Value` derives `Clone`, so every local-slot read (`LoadSlot`) and write
/// (`DefSlot`) clones a `Value` — three times per iteration of the 40k-loop
/// benchmark. When `Value::Int` held a bare `i64` that clone was a single 8-byte
/// move the optimizer could elide outright. With a bignum payload it has to
/// branch on the variant to decide whether an `Rc` refcount needs bumping, and
/// that branch alone measured ~15% slower VM throughput (the perf gate's
/// margin). Inlining puts the branch in the caller's basic block, where the
/// variant is usually already known, so the common `Small` case folds away.
impl Clone for BigNum {
    #[inline]
    fn clone(&self) -> Self {
        match self {
            BigNum::Small(n) => BigNum::Small(*n),
            BigNum::Big(b) => BigNum::Big(Rc::clone(b)),
        }
    }
}

impl BigNum {
    /// Wrap an `i64` in the small (inline) form.
    #[inline]
    pub fn small(n: i64) -> Self {
        BigNum::Small(n)
    }

    /// True if this value is held in the inline `i64` form.
    #[inline]
    pub fn is_small(&self) -> bool {
        matches!(self, BigNum::Small(_))
    }

    /// The `i64` value, or `None` if it does not fit (i.e. it is a `Big`).
    #[inline]
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            BigNum::Small(n) => Some(*n),
            BigNum::Big(_) => None,
        }
    }

    /// True if the value is zero (only `Small(0)` is zero).
    #[inline]
    pub fn is_zero(&self) -> bool {
        matches!(self, BigNum::Small(0))
    }

    /// True if the value is negative.
    #[inline]
    pub fn is_negative(&self) -> bool {
        match self {
            BigNum::Small(n) => *n < 0,
            BigNum::Big(b) => b.neg,
        }
    }

    /// Exact sum. `Small + Small` that overflows `i64` widens to `Big`.
    #[inline]
    pub fn add(&self, other: &BigNum) -> BigNum {
        match (self, other) {
            (BigNum::Small(a), BigNum::Small(b)) => match a.checked_add(*b) {
                Some(r) => BigNum::Small(r),
                None => big_add_signed(*a, *b),
            },
            _ => {
                let a = self.to_big();
                let b = other.to_big();
                big_add(&a, &b)
            }
        }
    }

    /// Exact difference.
    #[inline]
    pub fn sub(&self, other: &BigNum) -> BigNum {
        match (self, other) {
            (BigNum::Small(a), BigNum::Small(b)) => match a.checked_sub(*b) {
                Some(r) => BigNum::Small(r),
                // `i128`, not `-*b`: `a - i64::MIN` has no i64 answer and
                // `-i64::MIN` itself overflows, so negating in i64 would panic
                // instead of widening (this is `(- 0 -9223372036854775808)`).
                None => i128_to_bignum(*a as i128 - *b as i128),
            },
            _ => {
                let a = self.to_big();
                let b = other.to_big();
                big_sub(&a, &b)
            }
        }
    }

    /// Exact product.
    #[inline]
    pub fn mul(&self, other: &BigNum) -> BigNum {
        match (self, other) {
            (BigNum::Small(a), BigNum::Small(b)) => match a.checked_mul(*b) {
                Some(r) => BigNum::Small(r),
                None => big_mul_signed(*a, *b),
            },
            _ => {
                let a = self.to_big();
                let b = other.to_big();
                big_mul(&a, &b)
            }
        }
    }

    /// Exact negation. `Small(i64::MIN)` widens to `Big`.
    #[inline]
    pub fn neg(&self) -> BigNum {
        match self {
            BigNum::Small(n) => match n.checked_neg() {
                Some(r) => BigNum::Small(r),
                None => BigNum::Big(Rc::new(Big {
                    neg: false,
                    limbs: Rc::new(i64_to_limbs(*n)),
                })),
            },
            BigNum::Big(b) => BigNum::Big(Rc::new(Big {
                neg: !b.neg,
                limbs: Rc::clone(&b.limbs),
            })),
        }
    }

    /// Exact absolute value.
    #[inline]
    pub fn abs(&self) -> BigNum {
        match self {
            BigNum::Small(n) => match n.checked_abs() {
                Some(r) => BigNum::Small(r),
                None => BigNum::Big(Rc::new(Big {
                    neg: false,
                    limbs: Rc::new(i64_to_limbs(*n)),
                })),
            },
            BigNum::Big(b) => BigNum::Big(Rc::new(Big {
                neg: false,
                limbs: Rc::clone(&b.limbs),
            })),
        }
    }

    /// Euclidean remainder: result is in `[0, |other|)`. `other` must be
    /// non-zero (the caller checks). This is what `(mod a b)` uses.
    #[inline]
    pub fn rem_euclid(&self, other: &BigNum) -> BigNum {
        match (self, other) {
            (BigNum::Small(a), BigNum::Small(b)) => {
                // i64::MIN % -1 overflows the i64 intrinsic; the answer is 0.
                if *b == -1 {
                    BigNum::Small(0)
                } else {
                    BigNum::Small(a.rem_euclid(*b))
                }
            }
            _ => {
                let a = self.to_big();
                let b = other.to_big();
                big_rem_euclid(&a, &b)
            }
        }
    }

    /// Euclidean quotient, the `q` in `a = q * b + r` with `r` in
    /// `[0, |b|)`. `other` must be non-zero. (Exposed for completeness; `/`
    /// stays float, so the language's `mod` only needs `rem_euclid`.)
    #[inline]
    pub fn div_euclid(&self, other: &BigNum) -> BigNum {
        match (self, other) {
            (BigNum::Small(a), BigNum::Small(b)) => {
                // i64::MIN / -1 overflows the i64 intrinsic; the answer is
                // 2^63, which does not fit in i64.
                if *b == -1 {
                    self.neg()
                } else if *b == 1 {
                    BigNum::Small(*a)
                } else {
                    BigNum::Small(a.div_euclid(*b))
                }
            }
            _ => {
                let a = self.to_big();
                let b = other.to_big();
                big_div_euclid(&a, &b)
            }
        }
    }

    /// Total ordering (exact, even for out-of-`i64` values).
    ///
    /// Named `cmp_bignum` rather than `cmp` so it does not read as
    /// `Ord::cmp` when both are in scope at a call site (it takes `&BigNum`,
    /// not `&Self` for a type that does implement `Ord`, and clippy's
    /// `should_implement_trait` fires on the name alone).
    #[inline]
    #[allow(clippy::should_implement_trait)]
    pub fn cmp_bignum(&self, other: &BigNum) -> Ordering {
        match (self, other) {
            (BigNum::Small(a), BigNum::Small(b)) => a.cmp(b),
            _ => {
                let a = self.to_big();
                let b = other.to_big();
                big_cmp(&a, &b)
            }
        }
    }

    /// Best-effort `f64` (approximate for large magnitudes — `f64` has 53
    /// bits of integer precision). Used for int/float mixed arithmetic and
    /// int/float comparison, matching the old "promote to f64" behavior.
    #[inline]
    pub fn to_f64(&self) -> f64 {
        match self {
            BigNum::Small(n) => *n as f64,
            BigNum::Big(b) => {
                let mut v = 0.0f64;
                for &limb in b.limbs.iter().rev() {
                    v = v * (1u64 << 32) as f64 + limb as f64;
                }
                if b.neg {
                    -v
                } else {
                    v
                }
            }
        }
    }

    /// Parse a base-10 decimal string (optional leading `-`/`+`). Returns
    /// `None` if the text is not a valid integer.
    ///
    /// An inherent method rather than a `FromStr` impl: the parser returns
    /// `Option` (an invalid integer yields nothing) and the language has no
    /// error type to hand back here, so this is deliberately not the std trait.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<BigNum> {
        let s = s.trim();
        let (neg, digits) = if let Some(rest) = s.strip_prefix('-') {
            (true, rest)
        } else if let Some(rest) = s.strip_prefix('+') {
            (false, rest)
        } else {
            (false, s)
        };
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        // Reject leading zeros that would make the magnitude ambiguous? No —
        // "007" is a fine 7. But an all-zero digit string is zero.
        let mut mag: Vec<u32> = Vec::new();
        for ch in digits.bytes() {
            let d = (ch - b'0') as u32;
            // mag = mag * 10 + d
            let mut carry = d as u64;
            for limb in mag.iter_mut() {
                let cur = (*limb as u64) * 10 + carry;
                *limb = cur as u32;
                carry = cur >> 32;
            }
            while carry > 0 {
                mag.push((carry & 0xFFFF_FFFF) as u32);
                carry >>= 32;
            }
        }
        let mag = normalize(mag);
        if mag.is_empty() {
            return Some(BigNum::Small(0));
        }
        // Fast path: does it fit in i64?
        if mag.len() <= 2 {
            let lo = mag[0] as u64;
            let hi = mag.get(1).copied().unwrap_or(0) as u64;
            let m = (hi << 32) | lo;
            if !neg {
                if m <= i64::MAX as u64 {
                    return Some(BigNum::Small(m as i64));
                }
            } else if m <= (i64::MAX as u64) + 1 {
                // magnitude <= 2^63 -> representable as i64 (incl. MIN)
                return Some(BigNum::Small(-(m as i128) as i64));
            }
        }
        Some(BigNum::Big(Rc::new(Big {
            neg,
            limbs: Rc::new(mag),
        })))
    }

    /// Convert to a `Big` (wrapping a `Small` if needed). Used by the
    /// out-of-fast-path arithmetic.
    fn to_big(&self) -> Rc<Big> {
        match self {
            BigNum::Big(b) => Rc::clone(b),
            BigNum::Small(n) => Rc::new(Big {
                neg: *n < 0,
                limbs: Rc::new(i64_to_limbs(*n)),
            }),
        }
    }
}

impl From<i64> for BigNum {
    #[inline]
    fn from(n: i64) -> Self {
        BigNum::small(n)
    }
}

impl PartialEq for BigNum {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for BigNum {}
impl PartialOrd for BigNum {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for BigNum {
    fn cmp(&self, other: &Self) -> Ordering {
        self.cmp_bignum(other)
    }
}

impl fmt::Display for BigNum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BigNum::Small(n) => write!(f, "{n}"),
            BigNum::Big(b) => write!(f, "{}", big_to_string(b)),
        }
    }
}

impl fmt::Debug for BigNum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
    }
}

// ---- limb helpers (unsigned, little-endian base 2^32) ----------------------

/// Strip leading zero limbs. An empty result means the magnitude is zero.
fn normalize(mut v: Vec<u32>) -> Vec<u32> {
    while v.last() == Some(&0) {
        v.pop();
    }
    v
}

#[inline]
fn mag_is_zero(v: &[u32]) -> bool {
    v.iter().all(|&l| l == 0)
}

/// Unsigned compare of two magnitudes (different lengths allowed).
fn mag_cmp(a: &[u32], b: &[u32]) -> Ordering {
    let la = a.iter().rposition(|&l| l != 0).map(|i| i + 1).unwrap_or(0);
    let lb = b.iter().rposition(|&l| l != 0).map(|i| i + 1).unwrap_or(0);
    if la != lb {
        return la.cmp(&lb);
    }
    for i in (0..la).rev() {
        match a[i].cmp(&b[i]) {
            Ordering::Equal => {}
            o => return o,
        }
    }
    Ordering::Equal
}

#[inline]
fn mag_ge(a: &[u32], b: &[u32]) -> bool {
    mag_cmp(a, b) != Ordering::Less
}

/// Unsigned `a + b` (a, b little-endian).
fn mag_add(a: &[u32], b: &[u32]) -> Vec<u32> {
    let n = a.len().max(b.len());
    let mut out = Vec::with_capacity(n + 1);
    let mut carry: u64 = 0;
    for i in 0..n {
        let x = a.get(i).copied().unwrap_or(0) as u64;
        let y = b.get(i).copied().unwrap_or(0) as u64;
        let s = x + y + carry;
        out.push(s as u32);
        carry = s >> 32;
    }
    if carry > 0 {
        out.push(carry as u32);
    }
    out
}

/// Unsigned `a - b`, requiring `a >= b`.
fn mag_sub(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len());
    let mut borrow: i64 = 0;
    for (i, &limb) in a.iter().enumerate() {
        let x = limb as i64;
        let y = b.get(i).copied().unwrap_or(0) as i64;
        let d = x - y - borrow;
        if d < 0 {
            out.push((d + (1i64 << 32)) as u32);
            borrow = 1;
        } else {
            out.push(d as u32);
            borrow = 0;
        }
    }
    normalize(out)
}

/// Unsigned `a -= b` in place, requiring `a >= b`.
fn mag_sub_inplace(a: &mut [u32], b: &[u32]) {
    let mut borrow: i64 = 0;
    for (i, limb) in a.iter_mut().enumerate() {
        let x = *limb as i64;
        let y = b.get(i).copied().unwrap_or(0) as i64;
        let d = x - y - borrow;
        if d < 0 {
            *limb = (d + (1i64 << 32)) as u32;
            borrow = 1;
        } else {
            *limb = d as u32;
            borrow = 0;
        }
    }
}

/// Unsigned magnitude division by a small `u32` divisor. Returns
/// `(quotient, remainder)` with `a = q * d + r`, `0 <= r < d`.
fn mag_div_small(a: &[u32], d: u32) -> (Vec<u32>, u32) {
    let mut q = Vec::with_capacity(a.len());
    let mut rem: u64 = 0;
    for &limb in a.iter().rev() {
        let cur = (rem << 32) | limb as u64;
        q.push((cur / d as u64) as u32);
        rem = cur % d as u64;
    }
    q.reverse();
    (normalize(q), rem as u32)
}

/// Unsigned long division: `a = q * b + r`, `0 <= r < b`, `b != 0`.
///
/// Bit-by-bit (shift-subtract) — O(bits²) but big values are the rare case,
/// and correctness beats speed here.
fn mag_divmod(a: &[u32], b: &[u32]) -> (Vec<u32>, Vec<u32>) {
    let n = a.len();
    // r needs one extra limb: the invariant is r < 2*b after each shift, and
    // b can be up to n limbs, so 2*b can spill into limb n.
    let mut r = vec![0u32; n + 1];
    let mut q = vec![0u32; n];
    let total_bits = n * 32;
    for i in (0..total_bits).rev() {
        // shift r left by 1
        let mut carry = 0u32;
        for limb in r.iter_mut() {
            let new_carry = *limb >> 31;
            *limb = (*limb << 1) | carry;
            carry = new_carry;
        }
        // bring in bit i of a as the new LSB of r
        let abit = (a[i / 32] >> (i % 32)) & 1;
        r[0] |= abit;
        // if r >= b: r -= b; set quotient bit i
        if mag_ge(&r, b) {
            mag_sub_inplace(&mut r, b);
            q[i / 32] |= 1u32 << (i % 32);
        }
    }
    (normalize(q), normalize(r))
}

/// Convert an `i64` to its magnitude limbs (little-endian, normalized).
fn i64_to_limbs(n: i64) -> Vec<u32> {
    let m = n.unsigned_abs() as u128;
    let v = vec![(m & 0xFFFF_FFFF) as u32, ((m >> 32) & 0xFFFF_FFFF) as u32];
    normalize(v)
}

/// Render a `Big` as a decimal string.
fn big_to_string(b: &Big) -> String {
    let mut groups: Vec<u64> = Vec::new();
    let mut mag: Vec<u32> = b.limbs.to_vec();
    const BASE: u32 = 1_000_000_000;
    while !mag_is_zero(&mag) {
        let (q, r) = mag_div_small(&mag, BASE);
        groups.push(r as u64);
        mag = q;
    }
    let mut s = String::new();
    for (i, g) in groups.iter().rev().enumerate() {
        if i == 0 {
            s.push_str(&g.to_string());
        } else {
            s.push_str(&format!("{g:09}"));
        }
    }
    if b.neg {
        s.insert(0, '-');
    }
    s
}

// ---- signed-magnitude big arithmetic ---------------------------------------

/// Build a `BigNum` from a signed magnitude, narrowing to `Small` when the
/// value fits in `i64`. This keeps results in the allocation-free fast path
/// whenever possible (e.g. `(i64::MAX + 1) - 1` returns `Small(i64::MAX)`).
fn from_mag(mag: &[u32], neg: bool) -> BigNum {
    let len = mag
        .iter()
        .rposition(|&l| l != 0)
        .map(|i| i + 1)
        .unwrap_or(0);
    if len == 0 {
        return BigNum::Small(0);
    }
    if len > 2 {
        return BigNum::Big(Rc::new(Big {
            neg,
            limbs: Rc::new(mag.to_vec()),
        }));
    }
    let lo = mag[0] as u128;
    let hi = mag.get(1).copied().unwrap_or(0) as u128;
    let m = (hi << 32) | lo;
    if !neg {
        if m <= i64::MAX as u128 {
            return BigNum::Small(m as i64);
        }
    } else if m <= (i64::MAX as u128) + 1 {
        // |value| <= 2^63 -> representable as i64 (incl. i64::MIN)
        return BigNum::Small((-(m as i128)) as i64);
    }
    BigNum::Big(Rc::new(Big {
        neg,
        limbs: Rc::new(mag.to_vec()),
    }))
}

/// Add two signed-magnitude magnitudes that share a sign.
fn mag_add_signed(a: &Big, b: &Big, neg: bool) -> BigNum {
    let mag = mag_add(&a.limbs, &b.limbs);
    from_mag(&mag, neg)
}

fn big_add(a: &Big, b: &Big) -> BigNum {
    if a.neg == b.neg {
        mag_add_signed(a, b, a.neg)
    } else {
        // Opposite signs: subtract magnitudes, take the larger's sign.
        match mag_cmp(&a.limbs, &b.limbs) {
            Ordering::Equal => BigNum::Small(0),
            Ordering::Greater => {
                let mag = mag_sub(&a.limbs, &b.limbs);
                from_mag(&mag, a.neg)
            }
            Ordering::Less => {
                let mag = mag_sub(&b.limbs, &a.limbs);
                from_mag(&mag, b.neg)
            }
        }
    }
}

/// `a + b` where a, b are `i64` whose sum overflows `i64` (the rare
/// Small+Small overflow promotion).
fn big_add_signed(a: i64, b: i64) -> BigNum {
    // Use i128 to form the exact sum, then box it.
    let s = a as i128 + b as i128;
    i128_to_bignum(s)
}

/// Public form of `big_add_signed`: the widening half of the VM's `Add` hot
/// path, which tests the in-range `i64` case inline before calling this.
#[inline]
pub fn add_i64(a: i64, b: i64) -> BigNum {
    big_add_signed(a, b)
}

fn big_sub(a: &Big, b: &Big) -> BigNum {
    big_add(
        a,
        &Big {
            neg: !b.neg,
            limbs: Rc::clone(&b.limbs),
        },
    )
}

/// Unsigned magnitude product (schoolbook).
fn mag_mul(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = vec![0u32; a.len() + b.len()];
    for (i, &al) in a.iter().enumerate() {
        if al == 0 {
            continue;
        }
        let mut carry: u64 = 0;
        for (j, &bl) in b.iter().enumerate() {
            let cur = out[i + j] as u64 + al as u64 * bl as u64 + carry;
            out[i + j] = cur as u32;
            carry = cur >> 32;
        }
        let mut k = i + b.len();
        while carry > 0 {
            let cur = out[k] as u64 + carry;
            out[k] = cur as u32;
            carry = cur >> 32;
            k += 1;
        }
    }
    normalize(out)
}

fn big_mul(a: &Big, b: &Big) -> BigNum {
    let mag = mag_mul(&a.limbs, &b.limbs);
    from_mag(&mag, a.neg ^ b.neg)
}

/// `a * b` for the Small+Small overflow case (a, b `i64`).
fn big_mul_signed(a: i64, b: i64) -> BigNum {
    let p = a as i128 * b as i128;
    i128_to_bignum(p)
}

/// Public form of `big_mul_signed`: the widening half of the VM's `Mul` hot
/// path, which tests the in-range `i64` case inline before calling this.
#[inline]
pub fn mul_i64(a: i64, b: i64) -> BigNum {
    big_mul_signed(a, b)
}

fn big_cmp(a: &Big, b: &Big) -> Ordering {
    // Zero is never a Big, so both have a definite sign.
    if a.neg != b.neg {
        return if a.neg {
            Ordering::Less
        } else {
            Ordering::Greater
        };
    }
    let mag = mag_cmp(&a.limbs, &b.limbs);
    if a.neg {
        mag.reverse()
    } else {
        mag
    }
}

/// `a rem_euclid b` (b != 0). Result in `[0, |b|)`, always non-negative.
fn big_rem_euclid(a: &Big, b: &Big) -> BigNum {
    let (_, r_mag) = mag_divmod(&a.limbs, &b.limbs);
    // Truncated-division remainder has the sign of a. Euclidean adjustment:
    // if a is negative, r_euclid = |b| - r_mag (still in [0, |b|)).
    if a.neg {
        let mag = mag_sub(&b.limbs, &r_mag);
        from_mag(&mag, false)
    } else {
        from_mag(&r_mag, false)
    }
}

/// `a div_euclid b` (b != 0). The `q` in `a = q*b + r`, `r` in `[0, |b|)`.
fn big_div_euclid(a: &Big, b: &Big) -> BigNum {
    let (q_mag, r_mag) = mag_divmod(&a.limbs, &b.limbs);
    // Truncated quotient q_trunc has the sign of (a / b). Euclidean: if the
    // remainder was non-zero and a is negative, decrement the quotient by 1
    // (toward -inf) to keep r in [0, |b|).
    let mut q_neg = a.neg ^ b.neg;
    let mut q = q_mag;
    if !mag_is_zero(&r_mag) && a.neg {
        // q = q - 1 (in the truncated sense); adjust sign/magnitude.
        if q_neg {
            // q is negative; making it more negative increases magnitude.
            let mag = mag_add(&q, &[1]);
            q = mag;
        } else {
            // q is non-negative; decrement.
            if mag_is_zero(&q) {
                q = vec![1];
                q_neg = true;
            } else {
                q = mag_sub(&q, &[1]);
            }
        }
    }
    if mag_is_zero(&q) {
        BigNum::Small(0)
    } else {
        from_mag(&q, q_neg)
    }
}

/// Convert an `i128` to a `BigNum` (used by the Small+Small overflow paths).
fn i128_to_bignum(n: i128) -> BigNum {
    if ((i64::MIN as i128)..=(i64::MAX as i128)).contains(&n) {
        return BigNum::Small(n as i64);
    }
    let neg = n < 0;
    // `n` is out of i64 range here, so it is neither 0 nor i128::MIN and
    // negation in i128 is well-defined.
    let m: u128 = if neg { (-n) as u128 } else { n as u128 };
    let mut v = Vec::new();
    let mut x = m;
    while x > 0 {
        v.push((x & 0xFFFF_FFFF) as u32);
        x >>= 32;
    }
    if v.is_empty() {
        BigNum::Small(0)
    } else {
        BigNum::Big(Rc::new(Big {
            neg,
            limbs: Rc::new(v),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(n: i64) -> BigNum {
        BigNum::small(n)
    }

    #[test]
    fn small_fast_path_stays_inline() {
        assert!(s(1).add(&s(2)).is_small());
        assert_eq!(s(1).add(&s(2)), s(3));
        assert_eq!(s(5).sub(&s(8)), s(-3));
        assert_eq!(s(6).mul(&s(7)), s(42));
        assert_eq!(s(-7).rem_euclid(&s(3)), s(2));
        assert_eq!(s(7).rem_euclid(&s(-3)), s(1));
        assert_eq!(s(-7).rem_euclid(&s(-3)), s(2));
    }

    #[test]
    fn i64_max_times_two_is_exact() {
        // The card's headline case: i64::MAX * 2 == 18446744073709551614.
        let r = s(i64::MAX).mul(&s(2));
        assert!(!r.is_small());
        assert_eq!(r.to_string(), "18446744073709551614");
    }

    #[test]
    fn fact_25_is_exact() {
        let mut f = s(1);
        for i in 2..=25 {
            f = f.mul(&s(i));
        }
        assert_eq!(f.to_string(), "15511210043330985984000000");
    }

    #[test]
    fn fact_100_is_exact() {
        let mut f = s(1);
        for i in 2..=100 {
            f = f.mul(&s(i));
        }
        // 158 digits, the exact value Python/Ruby compute.
        assert_eq!(
            f.to_string(),
            "93326215443944152681699238856266700490715968264381621468592963895217599993229915608941463976156518286253697920827223758251185210916864000000000000000000000000"
        );
    }

    #[test]
    fn overflow_promotes_and_stays_exact() {
        // i64::MAX + 1 -> 9223372036854775808 (exact, no f64).
        assert_eq!(s(i64::MAX).add(&s(1)).to_string(), "9223372036854775808");
        // -i64::MIN -> 9223372036854775808.
        assert_eq!(s(i64::MIN).neg().to_string(), "9223372036854775808");
        // i64::MIN - 1 -> -9223372036854775809.
        assert_eq!(s(i64::MIN).sub(&s(1)).to_string(), "-9223372036854775809");
    }

    #[test]
    fn sub_of_i64_min_widens_instead_of_panicking() {
        // Regression: `0 - i64::MIN` has no i64 answer and `-i64::MIN` itself
        // overflows, so the overflow path must widen via i128 rather than
        // negate in i64. This used to panic ("attempt to negate with overflow").
        assert_eq!(s(0).sub(&s(i64::MIN)).to_string(), "9223372036854775808");
        // -1 - (-2^63) = 2^63 - 1, which fits i64 exactly (stays Small).
        assert_eq!(s(-1).sub(&s(i64::MIN)).to_string(), "9223372036854775807");
        assert_eq!(s(1).sub(&s(i64::MIN)).to_string(), "9223372036854775809");
        assert_eq!(
            s(i64::MAX).sub(&s(i64::MIN)).to_string(),
            "18446744073709551615"
        );
        // In-range results stay on the inline fast path.
        assert!(matches!(s(5).sub(&s(3)), BigNum::Small(2)));
    }

    #[test]
    fn big_add_sub_roundtrip() {
        let big = s(i64::MAX).add(&s(i64::MAX)); // 2^64 - 2
        assert_eq!(big.to_string(), "18446744073709551614");
        // big - (2*i64::MAX) == 0
        let two_max = s(i64::MAX).mul(&s(2));
        assert_eq!(big.sub(&two_max), s(0));
    }

    #[test]
    fn big_mul_negative() {
        let a = s(i64::MIN).neg(); // 2^63
        let b = s(i64::MIN); // -2^63
        let p = a.mul(&b); // -2^126
        assert!(p.is_negative());
        // |p| = 2^126
        assert_eq!(
            p.abs().to_string(),
            "85070591730234615865843651857942052864"
        );
    }

    #[test]
    fn cmp_across_variants() {
        let big = s(i64::MAX).add(&s(1)); // 2^63
        assert!(s(i64::MAX) < big);
        assert!(big < s(i64::MAX).add(&s(2)));
        assert_eq!(big.cmp(&s(i64::MAX).add(&s(1))), Ordering::Equal);
        // negative big vs small
        let neg_big = s(i64::MIN).sub(&s(1)); // -2^63 - 1
        assert!(neg_big < s(i64::MIN));
        assert!(neg_big < s(-1));
    }

    #[test]
    fn rem_euclid_by_one_is_zero() {
        assert_eq!(s(i64::MIN).rem_euclid(&s(-1)), s(0));
        let big = s(i64::MAX).mul(&s(2));
        assert_eq!(big.rem_euclid(&s(1)), s(0));
        assert_eq!(big.rem_euclid(&s(-1)), s(0));
    }

    #[test]
    fn rem_euclid_big() {
        // (2^64 - 2) mod 1000 == 614
        let big = s(i64::MAX).mul(&s(2));
        assert_eq!(big.rem_euclid(&s(1000)), s(614));
        // negative dividend: (-2^63 - 1) mod 7
        let neg = s(i64::MIN).sub(&s(1));
        let r = neg.rem_euclid(&s(7));
        // cross-check: must be in [0,7) and neg = 7*q + r
        let ri = r.as_i64().unwrap();
        assert!((0..7).contains(&ri));
        let q = neg.div_euclid(&s(7));
        let recon = q.mul(&s(7)).add(&r);
        assert_eq!(recon, neg);
    }

    #[test]
    fn div_euclid_identity() {
        for (a, b) in [
            (s(17), s(5)),
            (s(-17), s(5)),
            (s(17), s(-5)),
            (s(-17), s(-5)),
            (s(i64::MIN), s(3)),
        ] {
            let q = a.div_euclid(&b);
            let r = a.rem_euclid(&b);
            assert_eq!(q.mul(&b).add(&r), a, "a={a} b={b}");
            // Euclidean remainder is non-negative and < |b|
            assert!(!r.is_negative(), "a={a} b={b} r={r}");
            assert!(r.abs() < b.abs(), "a={a} b={b} r={r}");
        }
    }

    #[test]
    fn from_str_roundtrip() {
        for v in [
            "0",
            "1",
            "-1",
            "9223372036854775807",
            "-9223372036854775808",
            "9223372036854775808",
            "18446744073709551614",
            "15511210043330985984000000",
            "-15511210043330985984000000",
            "933262154439441526816992388562667004907159682643816214685929638952175999932299156089414639761565182862536979208272237582511852109168640000000000000000000000000000",
        ] {
            let p = BigNum::from_str(v).unwrap();
            assert_eq!(p.to_string(), v.trim_start_matches('+'), "roundtrip {v}");
        }
        assert!(BigNum::from_str("").is_none());
        assert!(BigNum::from_str("12a").is_none());
        assert!(BigNum::from_str("-").is_none());
        assert_eq!(BigNum::from_str("+42").unwrap(), s(42));
    }

    #[test]
    fn to_f64_approximate() {
        let big = s(i64::MAX).mul(&s(2));
        // 18446744073709551614 as f64 rounds to 18446744073709551616.0
        assert_eq!(big.to_f64(), 18446744073709551616.0f64);
        assert_eq!(s(42).to_f64(), 42.0);
    }

    #[test]
    fn big_string_large() {
        // 10^40
        let mut v = s(1);
        for _ in 0..40 {
            v = v.mul(&s(10));
        }
        assert_eq!(v.to_string(), "10000000000000000000000000000000000000000");
    }
}
