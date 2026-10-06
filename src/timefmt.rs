//! Wire formats for points in time. Instants are held as epoch
//! milliseconds; JSON carries them as RFC 3339 strings with
//! millisecond precision and a `Z` suffix, or, for chart series, as
//! epoch seconds in an `f64`.

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serializer;

/// RFC 3339 text of `dt`, e.g. `2026-10-06T12:00:00.000Z`.
pub fn rfc3339(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// RFC 3339 text of epoch milliseconds `ms`. A value outside chrono's
/// range reads as the epoch.
pub fn rfc3339_from_millis(ms: i64) -> String {
    rfc3339(DateTime::from_timestamp_millis(ms).unwrap_or_default())
}

/// RFC 3339 text of a protobuf timestamp, or `None` when it lies
/// outside chrono's range.
pub fn rfc3339_from_proto(ts: &prost_types::Timestamp) -> Option<String> {
    let nanos = u32::try_from(ts.nanos).ok()?;
    DateTime::from_timestamp(ts.seconds, nanos).map(rfc3339)
}

/// serde `serialize_with` for a `DateTime<Utc>`, written as
/// [`rfc3339`] text.
pub fn serialize_rfc3339<S: Serializer>(dt: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&rfc3339(*dt))
}

/// serde `serialize_with` for an `Option<DateTime<Utc>>`: [`rfc3339`]
/// text, or `null`.
pub fn serialize_opt_rfc3339<S: Serializer>(
    dt: &Option<DateTime<Utc>>,
    s: S,
) -> Result<S::Ok, S::Error> {
    match dt {
        Some(dt) => s.serialize_str(&rfc3339(*dt)),
        None => s.serialize_none(),
    }
}

/// serde `serialize_with` for an epoch-milliseconds `i64`, written as
/// an RFC 3339 string.
pub fn serialize_millis_as_rfc3339<S: Serializer>(ms: &i64, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&rfc3339_from_millis(*ms))
}

/// serde `serialize_with` for an epoch-milliseconds `i64`, written as
/// epoch seconds in an `f64`.
pub fn serialize_millis_as_epoch_s<S: Serializer>(ms: &i64, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_f64(*ms as f64 / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn millis_format_with_a_z_suffix() {
        assert_eq!(rfc3339_from_millis(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(
            rfc3339_from_millis(1_791_288_000_123),
            "2026-10-06T12:00:00.123Z"
        );
    }

    #[test]
    fn a_datetime_serializes_to_millis_with_a_z_suffix() {
        #[derive(serde::Serialize)]
        struct T {
            #[serde(serialize_with = "serialize_rfc3339")]
            at: DateTime<Utc>,
            #[serde(serialize_with = "serialize_opt_rfc3339")]
            some: Option<DateTime<Utc>>,
            #[serde(serialize_with = "serialize_opt_rfc3339")]
            none: Option<DateTime<Utc>>,
        }
        let at = DateTime::from_timestamp(1_791_288_000, 123_456_789).unwrap();
        let json = serde_json::to_string(&T {
            at,
            some: Some(at),
            none: None,
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"at":"2026-10-06T12:00:00.123Z","some":"2026-10-06T12:00:00.123Z","none":null}"#
        );
    }

    #[test]
    fn a_proto_timestamp_out_of_range_has_no_text() {
        let ts = prost_types::Timestamp {
            seconds: i64::MAX,
            nanos: 0,
        };
        assert_eq!(rfc3339_from_proto(&ts), None);
        let ts = prost_types::Timestamp {
            seconds: 1_791_288_000,
            nanos: 5_000_000,
        };
        assert_eq!(
            rfc3339_from_proto(&ts).as_deref(),
            Some("2026-10-06T12:00:00.005Z")
        );
    }
}
