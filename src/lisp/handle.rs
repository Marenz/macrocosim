//! `ComponentHandle` as a lisp value: an opaque host value that
//! round-trips through Lisp without losing its concrete type. The
//! `TulispAny` opt-in gives it the conversion for defun parameters,
//! return values and `AsList!` fields alike.

use tulisp::{TulispAny, TulispContext};

use crate::sim::ComponentHandle;

impl TulispAny for ComponentHandle {}

/// Helpers some of the make-* fns expose so config code can introspect a
/// handle (e.g. extract `id`).
pub fn register(ctx: &mut TulispContext) {
    ctx.defun("component-id", |h: ComponentHandle| -> i64 {
        h.id() as i64
    });
}
