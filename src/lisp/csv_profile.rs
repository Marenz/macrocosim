//! Time-indexed external load profile, loaded from a CSV file.
//!
//! CSV format expected:
//!
//!   time,kitchen,bedroom,office
//!   0,150,80,400
//!   60,200,75,420
//!   ...
//!
//! Column 0 is time in seconds (relative to whatever anchor the
//! caller chose); the remaining columns are values addressable by
//! header name. Values between sample rows are linearly interpolated.
//! Lookups before the first row return the first value, lookups
//! beyond the last return the last value — wrap-around is the
//! caller's job (a `(mod t period)` from Lisp does it cleanly).
//!
//! Stored in tulisp as an opaque `Shared<CsvLoadProfile>`; the only
//! operations are the `csv-*` defuns registered alongside.

use std::{
    borrow::Cow,
    collections::HashMap,
    fmt, fs,
    path::{Path, PathBuf},
};

use tulisp::{Error, Shared, TulispAny, TulispContext};

#[derive(Clone, Debug)]
pub struct CsvLoadProfile {
    /// Resolved path the file was loaded from. Just for Display.
    path: PathBuf,
    /// Sorted ascending. Values in `fields` are co-indexed.
    times: Vec<f64>,
    /// Header → column. Excludes the time column.
    fields: HashMap<String, Vec<f64>>,
}

impl fmt::Display for CsvLoadProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "<csv-profile {} rows={} fields=[{}]>",
            self.path.display(),
            self.times.len(),
            self.fields.keys().cloned().collect::<Vec<_>>().join(", ")
        )
    }
}

impl TulispAny for CsvLoadProfile {
    fn lisp_type_name() -> Cow<'static, str> {
        Cow::Borrowed("csv-profile")
    }
}

impl CsvLoadProfile {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        let raw = fs::read_to_string(path)
            .map_err(|e| Error::os_error(format!("csv-load {}: {}", path.display(), e)))?;

        let mut lines = raw.lines().filter(|l| !l.trim().is_empty());
        let header = lines.next().ok_or_else(|| {
            Error::invalid_argument(format!("csv-load {}: empty file", path.display()))
        })?;

        // First column must be time; the rest are value fields.
        let mut headers = header.split(',').map(str::trim);
        let time_col = headers.next().ok_or_else(|| {
            Error::invalid_argument(format!("csv-load {}: missing time column", path.display()))
        })?;
        if time_col.is_empty() {
            return Err(Error::invalid_argument(format!(
                "csv-load {}: blank header for time column",
                path.display()
            )));
        }
        let value_headers: Vec<String> = headers.map(|h| h.to_string()).collect();
        // Duplicate headers would collapse silently in the field
        // map below (last column wins) and serve the wrong data.
        let mut seen = std::collections::HashSet::new();
        for h in &value_headers {
            if !seen.insert(h) {
                return Err(Error::invalid_argument(format!(
                    "csv-load {}: duplicate column header '{h}'",
                    path.display()
                )));
            }
        }
        if value_headers.is_empty() {
            return Err(Error::invalid_argument(format!(
                "csv-load {}: no value columns after the time column",
                path.display()
            )));
        }

        let mut times = Vec::new();
        let mut columns: Vec<Vec<f64>> = vec![Vec::new(); value_headers.len()];
        for (lineno, line) in lines.enumerate() {
            let row = lineno + 2; // +1 for header, +1 for 1-based
            let mut cells = line.split(',').map(str::trim);
            let t = cells.next().ok_or_else(|| {
                Error::invalid_argument(format!("csv-load {}:{row}: empty row", path.display()))
            })?;
            let t: f64 = t.parse().map_err(|e| {
                Error::invalid_argument(format!(
                    "csv-load {}:{row}: time '{t}': {e}",
                    path.display()
                ))
            })?;
            if !t.is_finite() {
                return Err(Error::invalid_argument(format!(
                    "csv-load {}:{row}: time '{t}' is not finite",
                    path.display()
                )));
            }
            times.push(t);
            for (i, _) in value_headers.iter().enumerate() {
                let cell = cells.next().ok_or_else(|| {
                    Error::invalid_argument(format!(
                        "csv-load {}:{row}: missing column {}",
                        path.display(),
                        value_headers[i]
                    ))
                })?;
                let v: f64 = cell.parse().map_err(|e| {
                    Error::invalid_argument(format!(
                        "csv-load {}:{row} ({}): '{cell}': {e}",
                        path.display(),
                        value_headers[i]
                    ))
                })?;
                if !v.is_finite() {
                    return Err(Error::invalid_argument(format!(
                        "csv-load {}:{row} ({}): '{cell}' is not finite",
                        path.display(),
                        value_headers[i]
                    )));
                }
                columns[i].push(v);
            }
        }

        // Verify times are sorted ascending — interpolation assumes it.
        if times.windows(2).any(|w| w[0] > w[1]) {
            return Err(Error::invalid_argument(format!(
                "csv-load {}: time column must be ascending",
                path.display()
            )));
        }

        let fields = value_headers.into_iter().zip(columns).collect();
        Ok(Self {
            path: path.to_path_buf(),
            times,
            fields,
        })
    }

    pub fn fields(&self) -> Vec<String> {
        let mut v: Vec<String> = self.fields.keys().cloned().collect();
        v.sort();
        v
    }

    /// Linearly interpolate the named field at relative time `t`.
    /// Clamps to the endpoints when outside the sampled range — the
    /// caller is expected to apply `(mod t period)` if they want to
    /// loop.
    pub fn lookup(&self, field: &str, t: f64) -> Result<f64, Error> {
        let column = self.fields.get(field).ok_or_else(|| {
            Error::invalid_argument(format!(
                "csv-lookup: unknown field '{field}' in {}",
                self.path.display()
            ))
        })?;
        // A NaN time would fall through the endpoint clamps into the
        // binary search, whose partial_cmp unwrap panics — and the
        // caller is a lisp defun, so `t` is script-controlled. Reject
        // it like csv-load rejects non-finite cell values.
        if t.is_nan() {
            return Err(Error::invalid_argument(
                "csv-lookup: time must not be NaN".to_string(),
            ));
        }
        if self.times.is_empty() {
            return Ok(0.0);
        }
        // Endpoint clamps.
        if t <= self.times[0] {
            return Ok(column[0]);
        }
        let last = self.times.len() - 1;
        if t >= self.times[last] {
            return Ok(column[last]);
        }
        // Binary search for the interval [i, i+1] containing t.
        let i = match self.times.binary_search_by(|x| x.partial_cmp(&t).unwrap()) {
            Ok(exact) => return Ok(column[exact]),
            Err(insert) => insert - 1,
        };
        let t0 = self.times[i];
        let t1 = self.times[i + 1];
        let v0 = column[i];
        let v1 = column[i + 1];
        let frac = (t - t0) / (t1 - t0);
        Ok(v0 + frac * (v1 - v0))
    }
}

/// Register the `(csv-load)`, `(csv-fields)`, `(csv-lookup)` defuns.
pub fn register(ctx: &mut TulispContext, load_dir: std::path::PathBuf) {
    ctx.defun(
        "csv-load",
        move |path: String| -> Result<Shared<CsvLoadProfile>, Error> {
            // Resolve relative paths against the config's load dir,
            // like (load ...) and (file-exists-p ...) do — resolving
            // against the process CWD made the three disagree
            // whenever CWD ≠ config dir.
            let p = Path::new(&path);
            let resolved = if p.is_absolute() {
                p.to_path_buf()
            } else {
                load_dir.join(p)
            };
            let profile = CsvLoadProfile::load(&resolved)?;
            log::info!("csv-load: {}", profile);
            Ok(Shared::new(profile))
        },
    );

    ctx.defun("csv-fields", |profile: Shared<CsvLoadProfile>| {
        profile.fields()
    });

    ctx.defun(
        "csv-lookup",
        |profile: Shared<CsvLoadProfile>, field: String, t: f64| profile.lookup(&field, t),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A temp CSV file holding `contents`, removed when it is dropped.
    fn write_tmp(name: &str, contents: &str) -> tempfile::TempPath {
        let mut f = tempfile::Builder::new()
            .prefix(&format!("macrocosim-csv-{name}-"))
            .suffix(".csv")
            .tempfile()
            .unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        f.into_temp_path()
    }

    #[test]
    fn loads_and_interpolates() {
        let p = write_tmp("interp", "time,a,b\n0,100,500\n10,200,400\n20,300,300\n");
        let prof = CsvLoadProfile::load(&p).unwrap();
        assert_eq!(prof.fields(), vec!["a".to_string(), "b".to_string()]);

        // Exact rows
        assert!((prof.lookup("a", 0.0).unwrap() - 100.0).abs() < 1e-9);
        assert!((prof.lookup("a", 10.0).unwrap() - 200.0).abs() < 1e-9);
        assert!((prof.lookup("a", 20.0).unwrap() - 300.0).abs() < 1e-9);

        // Linear midpoint
        assert!((prof.lookup("a", 5.0).unwrap() - 150.0).abs() < 1e-9);
        assert!((prof.lookup("b", 5.0).unwrap() - 450.0).abs() < 1e-9);

        // Endpoint clamps
        assert!((prof.lookup("a", -10.0).unwrap() - 100.0).abs() < 1e-9);
        assert!((prof.lookup("a", 9999.0).unwrap() - 300.0).abs() < 1e-9);

        // Unknown field
        assert!(prof.lookup("missing", 0.0).is_err());
    }

    #[test]
    fn rejects_non_ascending_time() {
        let p = write_tmp("desc", "time,a\n10,1\n0,2\n");
        assert!(CsvLoadProfile::load(&p).is_err());
    }

    #[test]
    fn rejects_missing_value_column() {
        let p = write_tmp("missing", "time,a,b\n0,1,2\n10,3\n");
        assert!(CsvLoadProfile::load(&p).is_err());
    }

    #[test]
    fn lookup_rejects_nan_time_instead_of_panicking() {
        let p = write_tmp("nan-t", "time,a\n0,1\n10,2\n20,3\n");
        let prof = CsvLoadProfile::load(&p).unwrap();
        assert!(prof.lookup("a", f64::NAN).is_err());
        // Infinities take the endpoint clamps and stay fine.
        assert_eq!(prof.lookup("a", f64::INFINITY).unwrap(), 3.0);
        assert_eq!(prof.lookup("a", f64::NEG_INFINITY).unwrap(), 1.0);
    }
}
