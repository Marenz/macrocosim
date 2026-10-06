//! `LispValue`: a thin passthrough wrapper around `TulispObject` for
//! use as an `AsPlist!` field type.
//!
//! The blanket `From<T> for T` gives `TulispObject: TryFrom<TulispObject>`
//! the wrong error type (`Infallible`) for `AsPlist!`'s
//! `value.try_into()?`, which expects `Error = tulisp::Error`. This
//! newtype provides the right impls so a plist can carry a raw,
//! unparsed lisp value through to the make-* defun body — used by
//! `:power-w` and `:sunlight-pct`, where the make-* dispatcher inspects
//! the raw shape to decide between a constant and a Lisp-driven
//! [`DynamicScalar`](crate::sim::dynamic_scalar::DynamicScalar).

use tulisp::{Error, TulispObject};

#[derive(Clone, Debug)]
pub struct LispValue(TulispObject);

impl LispValue {
    pub fn into_inner(self) -> TulispObject {
        self.0
    }
    pub fn as_inner(&self) -> &TulispObject {
        &self.0
    }
}

impl TryFrom<TulispObject> for LispValue {
    type Error = Error;
    fn try_from(value: TulispObject) -> Result<Self, Self::Error> {
        Ok(Self(value))
    }
}

impl TryFrom<&TulispObject> for LispValue {
    type Error = Error;
    fn try_from(value: &TulispObject) -> Result<Self, Self::Error> {
        Ok(Self(value.clone()))
    }
}

/// `into_plist` direction — the `AsPlist!` macro requires this trait
/// bound even when we never call it.
impl From<LispValue> for TulispObject {
    fn from(v: LispValue) -> Self {
        v.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tulisp::TulispContext;

    #[test]
    fn passthrough_round_trip() {
        let mut ctx = TulispContext::new();
        let obj = ctx
            .eval_string("'((a . 1) (b . 2))")
            .expect("eval alist literal");
        let v: LispValue = obj.clone().try_into().unwrap();
        // The wrapper should hand back the same underlying object.
        assert!(v.as_inner().equal(&obj));
        let back: TulispObject = v.into();
        assert!(back.equal(&obj));
    }
}
