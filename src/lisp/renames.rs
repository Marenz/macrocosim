//! Old Lisp keyword names that still load. Each row maps an old
//! keyword to its new name and converts the value to the new unit.
//! Every defun that takes keywords parses through [`Renamed`], so a
//! managed file, a `*-defaults` plist or a script written with the
//! old names keeps working; each old name warns once per process.

use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};

use tulisp::{Error, Plistable, TulispContext, TulispObject};

/// How an old keyword's value becomes the new keyword's value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Convert {
    /// Same value, new name.
    None,
    /// Milliseconds to seconds (÷1000).
    MsToS,
    /// Kilowatt-hours to watt-hours (×1000).
    KwhToWh,
    /// A per-hour amount to per-second (÷3600).
    PerHourToPerSecond,
    /// A 0–1 fraction to percent (×100).
    FractionToPct,
    /// Events per hour to the mean gap between them in seconds
    /// (3600 ÷ x); 0 per hour stays 0, which means "none".
    RatePerHourToGapS,
}

impl Convert {
    /// The unit note the deprecation warning ends with.
    fn note(self) -> &'static str {
        match self {
            Convert::None => "",
            Convert::MsToS => " (seconds)",
            Convert::KwhToWh => " (Wh)",
            Convert::PerHourToPerSecond => " (per second)",
            Convert::FractionToPct => " (percent)",
            Convert::RatePerHourToGapS => " (mean seconds between events)",
        }
    }

    /// `value` in the new unit. `nil` and non-numbers pass through
    /// unchanged when no conversion is needed; a conversion on a
    /// non-number is an error naming the old keyword, except that a
    /// per-hour value may be a lambda or symbol, which becomes a
    /// lambda reading per second.
    fn apply(
        self,
        ctx: &mut TulispContext,
        old: &str,
        value: &TulispObject,
    ) -> Result<TulispObject, Error> {
        if self == Convert::None || value.null() {
            return Ok(value.clone());
        }
        if self == Convert::PerHourToPerSecond && !value.numberp() {
            return per_second_source(ctx, value);
        }
        if !value.numberp() {
            return Err(Error::invalid_argument(format!(
                "{old} expects a number, got {value}"
            )));
        }
        let x = value.try_float()?;
        let y = match self {
            Convert::None => x,
            Convert::MsToS => x / 1000.0,
            Convert::KwhToWh => x * 1000.0,
            Convert::PerHourToPerSecond => x / 3600.0,
            Convert::FractionToPct => x * 100.0,
            Convert::RatePerHourToGapS => {
                if x == 0.0 {
                    0.0
                } else {
                    3600.0 / x
                }
            }
        };
        Ok(TulispObject::from(y))
    }
}

/// A source of per-hour values read as per-second ones: the
/// unevaluated form `(/ V 3600.0)` for a symbol or expression `V`,
/// which prints back readably, and the compiled
/// `(lambda () (/ (funcall 'V) 3600.0))` for a function `V`.
fn per_second_source(ctx: &mut TulispContext, v: &TulispObject) -> Result<TulispObject, Error> {
    let list = |items: Vec<TulispObject>| items.into_iter().collect::<TulispObject>();
    if v.symbolp() || v.consp() {
        return Ok(list(vec![ctx.intern("/"), v.clone(), 3600.0.into()]));
    }
    let per_hour = list(vec![
        ctx.intern("funcall"),
        list(vec![ctx.intern("quote"), v.clone()]),
    ]);
    let form = list(vec![
        ctx.intern("lambda"),
        TulispObject::nil(),
        list(vec![ctx.intern("/"), per_hour, 3600.0.into()]),
    ]);
    ctx.eval(&form)
}

/// One renamed keyword.
pub(crate) struct Rename {
    pub old: &'static str,
    pub new: &'static str,
    pub convert: Convert,
}

/// Every renamed Lisp keyword. An old keyword means the same thing in
/// every defun that takes it.
pub(crate) const RENAMES: &[Rename] = &[
    Rename {
        old: ":interval",
        new: ":interval-s",
        convert: Convert::MsToS,
    },
    Rename {
        old: ":command-delay-ms",
        new: ":command-delay-s",
        convert: Convert::MsToS,
    },
    Rename {
        old: ":device-delay-ms",
        new: ":device-delay-s",
        convert: Convert::MsToS,
    },
    Rename {
        old: ":reactive-command-delay-ms",
        new: ":reactive-command-delay-s",
        convert: Convert::MsToS,
    },
    Rename {
        old: ":rated-fuse-current",
        new: ":rated-fuse-current-a",
        convert: Convert::None,
    },
    Rename {
        old: ":rated-lower",
        new: ":rated-lower-w",
        convert: Convert::None,
    },
    Rename {
        old: ":rated-upper",
        new: ":rated-upper-w",
        convert: Convert::None,
    },
    Rename {
        old: ":power",
        new: ":power-w",
        convert: Convert::None,
    },
    Rename {
        old: ":reactive-power",
        new: ":reactive-power-var",
        convert: Convert::None,
    },
    Rename {
        old: ":capacity",
        new: ":capacity-wh",
        convert: Convert::None,
    },
    Rename {
        old: ":voltage",
        new: ":voltage-v",
        convert: Convert::None,
    },
    Rename {
        old: ":ramp-rate",
        new: ":ramp-rate-w-per-s",
        convert: Convert::None,
    },
    Rename {
        old: ":reactive-ramp-rate",
        new: ":reactive-ramp-rate-var-per-s",
        convert: Convert::None,
    },
    Rename {
        old: ":initial-soc",
        new: ":initial-soc-pct",
        convert: Convert::None,
    },
    Rename {
        old: ":soc-lower",
        new: ":soc-lower-pct",
        convert: Convert::None,
    },
    Rename {
        old: ":soc-upper",
        new: ":soc-upper-pct",
        convert: Convert::None,
    },
    Rename {
        old: ":soc-protect-margin",
        new: ":soc-protect-margin-pct",
        convert: Convert::None,
    },
    Rename {
        old: ":soc",
        new: ":soc-pct",
        convert: Convert::None,
    },
    Rename {
        old: ":target-soc",
        new: ":target-soc-pct",
        convert: Convert::None,
    },
    Rename {
        old: ":capacity-kwh",
        new: ":capacity-wh",
        convert: Convert::KwhToWh,
    },
    Rename {
        old: ":taper-start",
        new: ":taper-start-pct",
        convert: Convert::None,
    },
    Rename {
        old: ":taper-floor",
        new: ":taper-floor-pct",
        convert: Convert::FractionToPct,
    },
    Rename {
        old: ":peak%",
        new: ":peak-pct",
        convert: Convert::None,
    },
    Rename {
        old: ":cloud-rate",
        new: ":cloud-mean-gap-s",
        convert: Convert::RatePerHourToGapS,
    },
    Rename {
        old: ":cloud-depth",
        new: ":cloud-depth-pct",
        convert: Convert::None,
    },
    Rename {
        old: ":cloud-duration",
        new: ":cloud-duration-s",
        convert: Convert::None,
    },
    Rename {
        old: ":cloud-ramp",
        new: ":cloud-ramp-s",
        convert: Convert::None,
    },
    Rename {
        old: ":nominal",
        new: ":nominal-hz",
        convert: Convert::None,
    },
    Rename {
        old: ":demand",
        new: ":demand-kg-per-s",
        convert: Convert::PerHourToPerSecond,
    },
    Rename {
        old: ":sunlight%",
        new: ":sunlight-pct",
        convert: Convert::None,
    },
];

/// Remembers which keys already warned.
struct WarnOnce(Mutex<HashSet<String>>);

impl WarnOnce {
    fn new() -> Self {
        WarnOnce(Mutex::new(HashSet::new()))
    }

    /// True the first time `key` is seen. A repeat allocates nothing.
    fn first(&self, key: &str) -> bool {
        let mut seen = self.0.lock().unwrap_or_else(|e| e.into_inner());
        !seen.contains(key) && seen.insert(key.to_string())
    }
}

static WARNED: LazyLock<WarnOnce> = LazyLock::new(WarnOnce::new);

/// Log `{old} is deprecated; use {new}{note}` the first time `old`
/// is seen in this process.
pub(crate) fn warn_renamed(old: &str, new: &str, note: &str) {
    warn_renamed_with(&WARNED, old, new, note);
}

fn warn_renamed_with(warned: &WarnOnce, old: &str, new: &str, note: &str) -> bool {
    let first = warned.first(old);
    if first {
        log::warn!("{old} is deprecated; use {new}{note}");
    }
    first
}

/// `kvs` with every old keyword in `table` replaced by its new name
/// and value. Order is kept, so last-one-wins still holds; an odd
/// tail is copied as is for the parser to reject.
fn rename_kvs_with(
    ctx: &mut TulispContext,
    kvs: &[TulispObject],
    table: &[Rename],
    warned: &WarnOnce,
) -> Result<Vec<TulispObject>, Error> {
    let mut out = Vec::with_capacity(kvs.len());
    let (pairs, tail) = kvs.as_chunks::<2>();
    for [key, value] in pairs {
        let row = key
            .keywordp()
            .then(|| key.to_string())
            .and_then(|name| table.iter().find(|r| r.old == name));
        match row {
            Some(r) => {
                warn_renamed_with(warned, r.old, r.new, r.convert.note());
                out.push(ctx.intern(r.new));
                out.push(r.convert.apply(ctx, r.old, value)?);
            }
            None => {
                out.push(key.clone());
                out.push(value.clone());
            }
        }
    }
    out.extend(tail.iter().cloned());
    Ok(out)
}

fn rename_kvs(ctx: &mut TulispContext, kvs: &[TulispObject]) -> Result<Vec<TulispObject>, Error> {
    rename_kvs_with(ctx, kvs, RENAMES, &WARNED)
}

/// A plist value (a `*-defaults` list) with old keywords renamed.
pub(crate) fn rename_plist_value(
    ctx: &mut TulispContext,
    plist: &TulispObject,
) -> Result<TulispObject, Error> {
    let kvs: Vec<TulispObject> = plist.base_iter().collect();
    Ok(rename_kvs(ctx, &kvs)?.into_iter().collect())
}

/// A defun's keyword arguments, parsed as `T` after old keywords are
/// renamed. Use as `Plist<Renamed<XArgs>>`.
pub(crate) struct Renamed<T>(pub T);

impl<T: Plistable> Plistable for Renamed<T> {
    fn from_plist_as_slice(ctx: &mut TulispContext, kvs: &[TulispObject]) -> Result<Self, Error> {
        let kvs = rename_kvs(ctx, kvs)?;
        T::from_plist_as_slice(ctx, &kvs).map(Renamed)
    }

    fn from_plist(ctx: &mut TulispContext, obj: &TulispObject) -> Result<Self, Error> {
        let kvs: Vec<TulispObject> = obj.base_iter().collect();
        Self::from_plist_as_slice(ctx, &kvs)
    }

    fn into_plist(self, ctx: &mut TulispContext) -> TulispObject {
        self.0.into_plist(ctx)
    }
}

/// Installs `(%warn-renamed OLD NEW NOTE)` for Lisp-defined functions
/// that accept an old keyword of their own, and
/// `(%kg-per-h-to-kg-per-s V)`, the per-hour to per-second conversion
/// of [`Convert::PerHourToPerSecond`] (`nil` stays `nil`).
pub(crate) fn register(ctx: &mut TulispContext) {
    ctx.defun("%warn-renamed", |old: String, new: String, note: String| {
        warn_renamed(&old, &new, &note);
        true
    });
    ctx.defun(
        "%kg-per-h-to-kg-per-s",
        |ctx: &mut TulispContext, v: TulispObject| -> Result<TulispObject, Error> {
            Convert::PerHourToPerSecond.apply(ctx, "%kg-per-h-to-kg-per-s", &v)
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &[Rename] = &[
        Rename {
            old: ":delay-ms",
            new: ":delay-s",
            convert: Convert::MsToS,
        },
        Rename {
            old: ":cap-kwh",
            new: ":cap-wh",
            convert: Convert::KwhToWh,
        },
        Rename {
            old: ":rate",
            new: ":gap-s",
            convert: Convert::RatePerHourToGapS,
        },
        Rename {
            old: ":label",
            new: ":name",
            convert: Convert::None,
        },
    ];

    fn renamed(src: &str) -> Result<String, Error> {
        let mut ctx = TulispContext::new();
        let list = ctx.eval_string(&format!("'{src}"))?;
        let kvs: Vec<TulispObject> = list.base_iter().collect();
        let out = rename_kvs_with(&mut ctx, &kvs, TABLE, &WarnOnce::new())?;
        Ok(out.into_iter().collect::<TulispObject>().to_string())
    }

    #[test]
    fn old_keywords_take_the_new_name_and_unit() {
        assert_eq!(
            renamed("(:delay-ms 200 :cap-kwh 60 :rate 3 :label \"x\")").unwrap(),
            "(:delay-s 0.2 :cap-wh 60000.0 :gap-s 1200.0 :name \"x\")"
        );
    }

    #[test]
    fn unknown_keywords_and_new_names_pass_through_in_order() {
        assert_eq!(
            renamed("(:delay-ms 500 :delay-s 2.0 :other 1)").unwrap(),
            "(:delay-s 0.5 :delay-s 2.0 :other 1)"
        );
    }

    #[test]
    fn a_zero_rate_stays_zero() {
        assert_eq!(renamed("(:rate 0)").unwrap(), "(:gap-s 0.0)");
    }

    #[test]
    fn nil_passes_through_a_conversion() {
        assert_eq!(renamed("(:delay-ms nil)").unwrap(), "(:delay-s nil)");
    }

    #[test]
    fn a_conversion_on_a_non_number_names_the_old_keyword() {
        let err = renamed("(:delay-ms \"soon\")").unwrap_err();
        assert!(
            err.to_string().contains(":delay-ms expects a number"),
            "{err}"
        );
    }

    #[test]
    fn an_odd_tail_is_left_for_the_parser() {
        assert_eq!(
            renamed("(:delay-ms 100 :dangling)").unwrap(),
            "(:delay-s 0.1 :dangling)"
        );
    }

    #[test]
    fn each_old_name_warns_once() {
        let warned = WarnOnce::new();
        assert!(warn_renamed_with(&warned, ":a", ":b", ""));
        assert!(!warn_renamed_with(&warned, ":a", ":b", ""));
        assert!(warn_renamed_with(&warned, ":c", ":d", ""));
    }

    #[test]
    fn warn_renamed_is_callable_from_lisp() {
        let mut ctx = TulispContext::new();
        register(&mut ctx);
        let v = ctx
            .eval_string(r#"(%warn-renamed ":old" ":new" " (seconds)")"#)
            .unwrap();
        assert!(!v.null());
    }
}
