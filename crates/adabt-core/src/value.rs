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
    Str(String),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Map(BTreeMap<String, Value>),
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
            Value::I64(_) | Value::U64(_) | Value::F64(_) => 2,
            Value::Str(_) => 3,
            Value::Bytes(_) => 4,
            Value::List(_) => 5,
            Value::Map(_) => 6,
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
            Value::Str(_) => "str",
            Value::Bytes(_) => "bytes",
            Value::List(_) => "list",
            Value::Map(_) => "map",
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Numeric widening used so that `I64(1)`, `U64(1)` and `F64(1.0)` compare
    /// equal. Returns `None` for non-numeric values.
    #[inline]
    fn as_num(&self) -> Option<f64> {
        match *self {
            Value::I64(v) => Some(v as f64),
            Value::U64(v) => Some(v as f64),
            Value::F64(v) => Some(v),
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
            (x, y) if x.rank() == 2 => {
                canon_f64(x.as_num().unwrap()).cmp(&canon_f64(y.as_num().unwrap()))
            }
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
            Value::I64(_) | Value::U64(_) | Value::F64(_) => {
                canon_f64(self.as_num().unwrap()).hash(state)
            }
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
