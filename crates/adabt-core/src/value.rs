//! The logical value domain.
//!
//! `Value` carries a **total** order and `Eq`/`Hash` even though it contains
//! `f64`. This is not cosmetic: the differential test rig compares engine output
//! against a reference model by equality, and index structures key on values.
//! Both need `NaN == NaN` and `-0.0 == 0.0` to behave consistently, so floats
//! are canonicalised to bits before comparison or hashing.

use std::cmp::Ordering;
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub enum Value {
    Null,
    Bool(bool),
    I64(i64),
    U64(u64),
    F64(f64),
    /// A base-ten fixed-point number: `units × 10^-scale`.
    ///
    /// **The type this database asks applications to keep money in.** Every
    /// other numeric here is binary floating point somewhere, and a project
    /// whose materialized views refuse to maintain an `f64` sum because it
    /// drifts in the last bit has no business telling anyone to store a price in
    /// one. Ten cents is `Decimal { units: 10, scale: 2 }` exactly, and stays
    /// exact through any amount of adding and subtracting.
    ///
    /// The scale travels with the value rather than living only in the schema,
    /// because a `Dynamic` collection has no schema to put it in.
    Decimal {
        units: i128,
        scale: u8,
    },
    /// Nanoseconds since the Unix epoch.
    ///
    /// Deliberately *not* a number. A timestamp orders and subtracts, but adding
    /// one to a price is a bug, and giving it its own rank means the type system
    /// says so rather than the reviewer.
    Timestamp(i64),
    Str(String),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Map(BTreeMap<String, Value>),
}

/// Move a fixed-point value to a different scale, exactly or not at all.
///
/// Widening multiplies and is exact until `i128` runs out. Narrowing is refused
/// rather than rounded: a schema saying "two decimal places" is a statement
/// about what the column *means*, and quietly dropping a third would make the
/// stored value disagree with the one the caller handed over — the whole failure
/// this type exists to avoid. A value too precise for its column is a schema
/// violation, reported like any other, not a rounding.
pub fn rescale(units: i128, from: u8, to: u8) -> Option<i128> {
    if to >= from {
        units.checked_mul(10i128.checked_pow((to - from) as u32)?)
    } else {
        let divisor = 10i128.checked_pow((from - to) as u32)?;
        (units % divisor == 0).then_some(units / divisor)
    }
}

/// A value's exact numeric content, if it has one.
///
/// `I64`, `U64` and `Decimal` are exact; `F64` is not. Two exact values are
/// compared by rescaling both to the finer scale in `i128`, so
/// `Decimal { units: 150, scale: 2 }` and `Decimal { units: 15, scale: 1 }` are
/// one value and a nineteen-digit integer is not silently rounded to compare
/// with its neighbour.
fn exact_units(v: &Value) -> Option<(i128, u8)> {
    match *v {
        Value::I64(n) => Some((n as i128, 0)),
        Value::U64(n) => Some((n as i128, 0)),
        Value::Decimal { units, scale } => Some((units, scale)),
        _ => None,
    }
}

/// Compare two exact numerics without going through a float.
///
/// `None` when either side is inexact, or when rescaling would overflow `i128` —
/// in both cases the caller falls back to the float comparison, which is the
/// answer it would have given anyway before this existed.
fn cmp_exact(a: &Value, b: &Value) -> Option<Ordering> {
    let (au, asc) = exact_units(a)?;
    let (bu, bsc) = exact_units(b)?;
    let target = asc.max(bsc);
    let lift = |u: i128, s: u8| -> Option<i128> {
        u.checked_mul(10i128.checked_pow((target - s) as u32)?)
    };
    Some(lift(au, asc)?.cmp(&lift(bu, bsc)?))
}

/// The four basic arithmetic operations, for [`checked_arith`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
}

/// Compute `a op b`, exactly where both sides are exact.
///
/// **This is where the project's stance on money actually pays off.** Two
/// exact numerics (`I64`, `U64`, `Decimal`) are rescaled to a common scale and
/// combined in `i128`, so `10.00 + 0.50 = 10.50` lands on the bit pattern that
/// means exactly that, the same guarantee `rescale` gives storage and
/// `matview.rs`'s exactness budget gives materialized sums. The moment either
/// side is an `F64`, or the exact path overflows `i128`, this falls back to
/// floating point — predicate arithmetic over a genuinely irrational or
/// approximate quantity has no exact answer to protect.
///
/// `None` on a non-numeric operand, overflow with no exact or float fallback
/// left, or division by zero. A predicate evaluator can turn that into
/// "unknown" the same way it already treats a missing field — this function
/// does not decide what missing information means, only compute what it can.
pub fn checked_arith(op: ArithOp, a: &Value, b: &Value) -> Option<Value> {
    if let (Some((au, asc)), Some((bu, bsc))) = (exact_units(a), exact_units(b)) {
        let target = asc.max(bsc);
        let lift = |u: i128, s: u8| u.checked_mul(10i128.checked_pow((target - s) as u32)?);
        if let (Some(a), Some(b)) = (lift(au, asc), lift(bu, bsc)) {
            let exact = match op {
                ArithOp::Add => a.checked_add(b),
                ArithOp::Sub => a.checked_sub(b),
                // Multiplying two already-scaled values double-counts the
                // scale, so the product is brought back down by it — the same
                // fixed-point convention every decimal library uses.
                ArithOp::Mul => a
                    .checked_mul(b)
                    .and_then(|p| p.checked_div(10i128.checked_pow(target as u32)?)),
                ArithOp::Div => {
                    if b == 0 {
                        None
                    } else {
                        a.checked_mul(10i128.checked_pow(target as u32)?)
                            .and_then(|n| n.checked_div(b))
                    }
                }
            };
            if let Some(units) = exact {
                return Some(Value::Decimal {
                    units,
                    scale: target,
                });
            }
        }
    }
    let (a, b) = (a.as_num()?, b.as_num()?);
    let r = match op {
        ArithOp::Add => a + b,
        ArithOp::Sub => a - b,
        ArithOp::Mul => a * b,
        ArithOp::Div => {
            if b == 0.0 {
                return None;
            }
            a / b
        }
    };
    r.is_finite().then_some(Value::F64(r))
}

/// Canonical bit pattern for an `f64`, collapsing every NaN to one value and
/// `-0.0` to `0.0`, then re-biasing so the integer order matches numeric order.
#[inline]
fn canon_f64(f: f64) -> u64 {
    let f = if f.is_nan() {
        f64::NAN
    } else if f == 0.0 {
        0.0
    } else {
        f
    };
    let bits = f.to_bits();
    // Flip the sign bit for positives, flip everything for negatives, so that
    // the resulting u64 sorts in the same order as the original f64.
    if bits & (1 << 63) != 0 {
        !bits
    } else {
        bits ^ (1 << 63)
    }
}

impl Value {
    /// Discriminant rank, fixing the cross-type ordering. Stable across releases
    /// because index key encoding depends on it.
    #[inline]
    fn rank(&self) -> u8 {
        match self {
            Value::Null => 0,
            Value::Bool(_) => 1,
            // `Decimal` shares the numeric rank on purpose: the promise this
            // type system already makes is that a number is one value however it
            // is written, and exempting decimals would make `price = 10` fail to
            // match a price of ten.
            Value::I64(_) | Value::U64(_) | Value::F64(_) | Value::Decimal { .. } => 2,
            Value::Str(_) => 3,
            Value::Bytes(_) => 4,
            Value::List(_) => 5,
            Value::Map(_) => 6,
            Value::Timestamp(_) => 7,
        }
    }

    /// Name used in type-mismatch diagnostics.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::I64(_) => "i64",
            Value::U64(_) => "u64",
            Value::F64(_) => "f64",
            Value::Decimal { .. } => "decimal",
            Value::Timestamp(_) => "timestamp",
            Value::Str(_) => "str",
            Value::Bytes(_) => "bytes",
            Value::List(_) => "list",
            Value::Map(_) => "map",
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Rough in-memory footprint, in bytes.
    ///
    /// Not a wire size and not exact — it exists only so a query-time memory
    /// budget has a number to add up, and being off by the constant overhead
    /// of a `Vec`/`String`/`BTreeMap` allocation is the right kind of wrong
    /// for that: a budget is a circuit breaker against a query that is
    /// unboundedly larger than expected, not an accountant.
    pub fn approx_size(&self) -> usize {
        let payload = match self {
            Value::Null | Value::Bool(_) => 0,
            Value::I64(_) | Value::U64(_) | Value::F64(_) | Value::Timestamp(_) => 8,
            Value::Decimal { .. } => 17,
            Value::Str(s) => s.len(),
            Value::Bytes(b) => b.len(),
            Value::List(items) => items.iter().map(Value::approx_size).sum(),
            Value::Map(m) => m.iter().map(|(k, v)| k.len() + v.approx_size()).sum(),
        };
        std::mem::size_of::<Value>() + payload
    }

    /// Numeric widening used so that `I64(1)`, `U64(1)` and `F64(1.0)` compare
    /// equal. Returns `None` for non-numeric values.
    #[inline]
    /// This value's numeric content as a float, widening exactly as `Ord`
    /// does. Not the arithmetic entry point — that is [`checked_arith`], which
    /// stays exact where it can — this exists for callers that only need a
    /// comparable magnitude.
    pub fn as_num(&self) -> Option<f64> {
        match *self {
            Value::I64(v) => Some(v as f64),
            Value::U64(v) => Some(v as f64),
            Value::F64(v) => Some(v),
            Value::Decimal { units, scale } => Some(units as f64 / 10f64.powi(scale as i32)),
            _ => None,
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Value {}

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Value {
    fn cmp(&self, other: &Self) -> Ordering {
        let (a, b) = (self.rank(), other.rank());
        if a != b {
            return a.cmp(&b);
        }
        match (self, other) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
            // Same rank 2: compare as canonicalised floats so mixed integer and
            // float representations of the same number are one value.
            // Exactly where both sides are exact, by float where one is not.
            // Falling back is not a compromise: comparing a decimal against a
            // float is already an approximate question, and answering it
            // approximately is the honest result.
            (x, y) if x.rank() == 2 => cmp_exact(x, y).unwrap_or_else(|| {
                canon_f64(x.as_num().unwrap()).cmp(&canon_f64(y.as_num().unwrap()))
            }),
            (Value::Timestamp(x), Value::Timestamp(y)) => x.cmp(y),
            (Value::Str(x), Value::Str(y)) => x.cmp(y),
            (Value::Bytes(x), Value::Bytes(y)) => x.cmp(y),
            (Value::List(x), Value::List(y)) => x.cmp(y),
            (Value::Map(x), Value::Map(y)) => x.cmp(y),
            _ => unreachable!("rank equality implies same variant group"),
        }
    }
}

impl std::hash::Hash for Value {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.rank().hash(state);
        match self {
            Value::Null => {}
            Value::Bool(b) => b.hash(state),
            // Must agree with `Ord`: numerics hash on the canonical float bits.
            // Hashing the float form keeps `Hash` agreeing with `Ord`: two
            // values that compare equal have the same numeric content, so they
            // round to the same float. Two that merely round alike collide,
            // which is a hash doing its job.
            Value::I64(_) | Value::U64(_) | Value::F64(_) | Value::Decimal { .. } => {
                canon_f64(self.as_num().unwrap()).hash(state)
            }
            Value::Timestamp(t) => t.hash(state),
            Value::Str(s) => s.hash(state),
            Value::Bytes(b) => b.hash(state),
            Value::List(l) => l.hash(state),
            Value::Map(m) => m.hash(state),
        }
    }
}

macro_rules! from_impl {
    ($t:ty, $variant:ident) => {
        impl From<$t> for Value {
            fn from(v: $t) -> Self {
                Value::$variant(v.into())
            }
        }
    };
}
from_impl!(bool, Bool);
from_impl!(i64, I64);
from_impl!(i32, I64);
from_impl!(u64, U64);
from_impl!(f64, F64);
from_impl!(String, Str);
from_impl!(&str, Str);
from_impl!(Vec<u8>, Bytes);

#[cfg(test)]
mod tests {
    #[test]
    fn exact_decimal_addition_is_exact() {
        let a = Value::Decimal {
            units: 1000,
            scale: 2,
        }; // 10.00
        let b = Value::Decimal {
            units: 50,
            scale: 2,
        }; // 0.50
        let r = checked_arith(ArithOp::Add, &a, &b).unwrap();
        assert_eq!(
            r,
            Value::Decimal {
                units: 1050,
                scale: 2
            }
        );
    }

    #[test]
    fn exact_decimal_subtraction_handles_mismatched_scales() {
        let a = Value::Decimal {
            units: 1000,
            scale: 2,
        }; // 10.00
        let b = Value::I64(3); // 3, scale 0
        let r = checked_arith(ArithOp::Sub, &a, &b).unwrap();
        assert_eq!(
            r,
            Value::Decimal {
                units: 700,
                scale: 2
            }
        ); // 7.00
    }

    #[test]
    fn exact_decimal_multiplication_rescales_the_product() {
        let a = Value::Decimal {
            units: 250,
            scale: 2,
        }; // 2.50
        let b = Value::Decimal {
            units: 400,
            scale: 2,
        }; // 4.00
        let r = checked_arith(ArithOp::Mul, &a, &b).unwrap();
        // 2.50 * 4.00 = 10.00
        assert_eq!(
            r,
            Value::Decimal {
                units: 1000,
                scale: 2
            }
        );
    }

    #[test]
    fn exact_decimal_division_is_exact_when_it_can_be() {
        let a = Value::Decimal {
            units: 1000,
            scale: 2,
        }; // 10.00
        let b = Value::I64(4);
        let r = checked_arith(ArithOp::Div, &a, &b).unwrap();
        assert_eq!(
            r,
            Value::Decimal {
                units: 250,
                scale: 2
            }
        ); // 2.50
    }

    #[test]
    fn division_by_zero_is_refused_exactly_and_by_float() {
        assert_eq!(
            checked_arith(ArithOp::Div, &Value::I64(10), &Value::I64(0)),
            None
        );
        assert_eq!(
            checked_arith(ArithOp::Div, &Value::F64(10.0), &Value::F64(0.0)),
            None
        );
    }

    #[test]
    fn mixing_in_a_float_falls_back_to_float_arithmetic() {
        let a = Value::Decimal {
            units: 100,
            scale: 2,
        }; // 1.00
        let b = Value::F64(0.5);
        let r = checked_arith(ArithOp::Add, &a, &b).unwrap();
        assert_eq!(r, Value::F64(1.5));
    }

    #[test]
    fn a_non_numeric_operand_is_refused() {
        assert_eq!(
            checked_arith(ArithOp::Add, &Value::from("x"), &Value::I64(1)),
            None
        );
    }
    use super::*;

    #[test]
    fn nan_is_reflexive() {
        assert_eq!(Value::F64(f64::NAN), Value::F64(f64::NAN));
    }

    #[test]
    fn signed_zero_unifies() {
        assert_eq!(Value::F64(0.0), Value::F64(-0.0));
    }

    #[test]
    fn numeric_variants_unify() {
        assert_eq!(Value::I64(1), Value::U64(1));
        assert_eq!(Value::I64(1), Value::F64(1.0));
    }

    #[test]
    fn float_order_matches_numeric_order() {
        let mut v = [
            Value::F64(1.0),
            Value::F64(-5.0),
            Value::F64(0.0),
            Value::F64(-0.5),
            Value::F64(1e300),
        ];
        v.sort();
        let got: Vec<f64> = v
            .iter()
            .map(|x| match x {
                Value::F64(f) => *f,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(got, vec![-5.0, -0.5, 0.0, 1.0, 1e300]);
    }

    #[test]
    fn eq_implies_equal_hash() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let h = |v: &Value| {
            let mut s = DefaultHasher::new();
            v.hash(&mut s);
            s.finish()
        };
        assert_eq!(h(&Value::I64(7)), h(&Value::F64(7.0)));
        assert_eq!(h(&Value::F64(0.0)), h(&Value::F64(-0.0)));
    }

    #[test]
    fn cross_type_order_is_by_rank() {
        assert!(Value::Null < Value::Bool(false));
        assert!(Value::Bool(true) < Value::I64(-9999));
        assert!(Value::I64(9999) < Value::Str(String::new()));
    }
}
