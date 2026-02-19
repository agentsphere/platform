//! Minimal OTLP protobuf types using prost derive macros.
//!
//! Defines the subset of OpenTelemetry Protocol messages needed for
//! traces, logs, and metrics ingest over HTTP protobuf.

use prost::Message;

// ---------------------------------------------------------------------------
// Common types
// ---------------------------------------------------------------------------

#[derive(Clone, Message)]
pub struct KeyValue {
    #[prost(string, tag = "1")]
    pub key: String,
    #[prost(message, optional, tag = "2")]
    pub value: Option<AnyValue>,
}

#[derive(Clone, Message)]
pub struct AnyValue {
    #[prost(oneof = "any_value::Value", tags = "1, 2, 3, 4, 5, 6")]
    pub value: Option<any_value::Value>,
}

pub mod any_value {
    #[derive(Clone, prost::Oneof)]
    #[allow(clippy::enum_variant_names)]
    pub enum Value {
        #[prost(string, tag = "1")]
        StringValue(String),
        #[prost(bool, tag = "2")]
        BoolValue(bool),
        #[prost(int64, tag = "3")]
        IntValue(i64),
        #[prost(double, tag = "4")]
        DoubleValue(f64),
        #[prost(message, tag = "5")]
        ArrayValue(super::ArrayValue),
        #[prost(message, tag = "6")]
        KvlistValue(super::KeyValueList),
    }
}

#[derive(Clone, Message)]
pub struct ArrayValue {
    #[prost(message, repeated, tag = "1")]
    pub values: Vec<AnyValue>,
}

#[derive(Clone, Message)]
pub struct KeyValueList {
    #[prost(message, repeated, tag = "1")]
    pub values: Vec<KeyValue>,
}

#[derive(Clone, Message)]
pub struct Resource {
    #[prost(message, repeated, tag = "1")]
    pub attributes: Vec<KeyValue>,
}

#[derive(Clone, Message)]
pub struct InstrumentationScope {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub version: String,
}

// ---------------------------------------------------------------------------
// Traces
// ---------------------------------------------------------------------------

#[derive(Clone, Message)]
pub struct ExportTraceServiceRequest {
    #[prost(message, repeated, tag = "1")]
    pub resource_spans: Vec<ResourceSpans>,
}

#[derive(Clone, Message)]
pub struct ExportTraceServiceResponse {}

#[derive(Clone, Message)]
pub struct ResourceSpans {
    #[prost(message, optional, tag = "1")]
    pub resource: Option<Resource>,
    #[prost(message, repeated, tag = "2")]
    pub scope_spans: Vec<ScopeSpans>,
}

#[derive(Clone, Message)]
pub struct ScopeSpans {
    #[prost(message, optional, tag = "1")]
    pub scope: Option<InstrumentationScope>,
    #[prost(message, repeated, tag = "2")]
    pub spans: Vec<Span>,
}

#[derive(Clone, Message)]
#[allow(clippy::struct_field_names)]
pub struct Span {
    #[prost(bytes = "vec", tag = "1")]
    pub trace_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "2")]
    pub span_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "4")]
    pub parent_span_id: Vec<u8>,
    #[prost(string, tag = "5")]
    pub name: String,
    #[prost(enumeration = "SpanKind", tag = "6")]
    pub kind: i32,
    #[prost(fixed64, tag = "7")]
    pub start_time_unix_nano: u64,
    #[prost(fixed64, tag = "8")]
    pub end_time_unix_nano: u64,
    #[prost(message, repeated, tag = "9")]
    pub attributes: Vec<KeyValue>,
    #[prost(message, repeated, tag = "11")]
    pub events: Vec<SpanEvent>,
    #[prost(message, optional, tag = "14")]
    pub status: Option<SpanStatus>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
pub enum SpanKind {
    Unspecified = 0,
    Internal = 1,
    Server = 2,
    Client = 3,
    Producer = 4,
    Consumer = 5,
}

#[derive(Clone, Message)]
pub struct SpanEvent {
    #[prost(fixed64, tag = "1")]
    pub time_unix_nano: u64,
    #[prost(string, tag = "2")]
    pub name: String,
    #[prost(message, repeated, tag = "3")]
    pub attributes: Vec<KeyValue>,
}

#[derive(Clone, Message)]
pub struct SpanStatus {
    #[prost(string, tag = "2")]
    pub message: String,
    #[prost(enumeration = "StatusCode", tag = "3")]
    pub code: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
pub enum StatusCode {
    Unset = 0,
    Ok = 1,
    Error = 2,
}

// ---------------------------------------------------------------------------
// Logs
// ---------------------------------------------------------------------------

#[derive(Clone, Message)]
pub struct ExportLogsServiceRequest {
    #[prost(message, repeated, tag = "1")]
    pub resource_logs: Vec<ResourceLogs>,
}

#[derive(Clone, Message)]
pub struct ExportLogsServiceResponse {}

#[derive(Clone, Message)]
pub struct ResourceLogs {
    #[prost(message, optional, tag = "1")]
    pub resource: Option<Resource>,
    #[prost(message, repeated, tag = "2")]
    pub scope_logs: Vec<ScopeLogs>,
}

#[derive(Clone, Message)]
pub struct ScopeLogs {
    #[prost(message, optional, tag = "1")]
    pub scope: Option<InstrumentationScope>,
    #[prost(message, repeated, tag = "2")]
    pub log_records: Vec<LogRecord>,
}

#[derive(Clone, Message)]
pub struct LogRecord {
    #[prost(fixed64, tag = "1")]
    pub time_unix_nano: u64,
    #[prost(enumeration = "SeverityNumber", tag = "2")]
    pub severity_number: i32,
    #[prost(string, tag = "3")]
    pub severity_text: String,
    #[prost(message, optional, tag = "5")]
    pub body: Option<AnyValue>,
    #[prost(message, repeated, tag = "6")]
    pub attributes: Vec<KeyValue>,
    #[prost(bytes = "vec", tag = "9")]
    pub trace_id: Vec<u8>,
    #[prost(bytes = "vec", tag = "10")]
    pub span_id: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
pub enum SeverityNumber {
    Unspecified = 0,
    Trace = 1,
    Trace2 = 2,
    Trace3 = 3,
    Trace4 = 4,
    Debug = 5,
    Debug2 = 6,
    Debug3 = 7,
    Debug4 = 8,
    Info = 9,
    Info2 = 10,
    Info3 = 11,
    Info4 = 12,
    Warn = 13,
    Warn2 = 14,
    Warn3 = 15,
    Warn4 = 16,
    Error = 17,
    Error2 = 18,
    Error3 = 19,
    Error4 = 20,
    Fatal = 21,
    Fatal2 = 22,
    Fatal3 = 23,
    Fatal4 = 24,
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

#[derive(Clone, Message)]
pub struct ExportMetricsServiceRequest {
    #[prost(message, repeated, tag = "1")]
    pub resource_metrics: Vec<ResourceMetrics>,
}

#[derive(Clone, Message)]
pub struct ExportMetricsServiceResponse {}

#[derive(Clone, Message)]
pub struct ResourceMetrics {
    #[prost(message, optional, tag = "1")]
    pub resource: Option<Resource>,
    #[prost(message, repeated, tag = "2")]
    pub scope_metrics: Vec<ScopeMetrics>,
}

#[derive(Clone, Message)]
pub struct ScopeMetrics {
    #[prost(message, optional, tag = "1")]
    pub scope: Option<InstrumentationScope>,
    #[prost(message, repeated, tag = "2")]
    pub metrics: Vec<Metric>,
}

#[derive(Clone, Message)]
pub struct Metric {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub description: String,
    #[prost(string, tag = "3")]
    pub unit: String,
    #[prost(oneof = "metric_data::Data", tags = "5, 7, 9")]
    pub data: Option<metric_data::Data>,
}

pub mod metric_data {
    #[derive(Clone, prost::Oneof)]
    pub enum Data {
        #[prost(message, tag = "5")]
        Gauge(super::Gauge),
        #[prost(message, tag = "7")]
        Sum(super::Sum),
        #[prost(message, tag = "9")]
        Histogram(super::Histogram),
    }
}

#[derive(Clone, Message)]
pub struct Gauge {
    #[prost(message, repeated, tag = "1")]
    pub data_points: Vec<NumberDataPoint>,
}

#[derive(Clone, Message)]
pub struct Sum {
    #[prost(message, repeated, tag = "1")]
    pub data_points: Vec<NumberDataPoint>,
    #[prost(bool, tag = "3")]
    pub is_monotonic: bool,
}

#[derive(Clone, Message)]
pub struct Histogram {
    #[prost(message, repeated, tag = "1")]
    pub data_points: Vec<HistogramDataPoint>,
}

#[derive(Clone, Message)]
pub struct NumberDataPoint {
    #[prost(message, repeated, tag = "7")]
    pub attributes: Vec<KeyValue>,
    #[prost(fixed64, tag = "3")]
    pub time_unix_nano: u64,
    #[prost(oneof = "number_data_point::Value", tags = "4, 6")]
    pub value: Option<number_data_point::Value>,
}

pub mod number_data_point {
    #[derive(Clone, prost::Oneof)]
    pub enum Value {
        #[prost(double, tag = "4")]
        AsDouble(f64),
        #[prost(sfixed64, tag = "6")]
        AsInt(i64),
    }
}

#[derive(Clone, Message)]
pub struct HistogramDataPoint {
    #[prost(message, repeated, tag = "9")]
    pub attributes: Vec<KeyValue>,
    #[prost(fixed64, tag = "3")]
    pub time_unix_nano: u64,
    #[prost(fixed64, tag = "4")]
    pub count: u64,
    #[prost(double, optional, tag = "5")]
    pub sum: Option<f64>,
    #[prost(double, repeated, tag = "7")]
    pub explicit_bounds: Vec<f64>,
    #[prost(fixed64, repeated, tag = "6")]
    pub bucket_counts: Vec<u64>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert 16-byte `trace_id` to hex string.
pub fn trace_id_to_hex(bytes: &[u8]) -> String {
    hex::encode(bytes)
}

/// Convert 8-byte `span_id` to hex string.
pub fn span_id_to_hex(bytes: &[u8]) -> String {
    hex::encode(bytes)
}

/// Extract a string attribute value from a `KeyValue` slice.
pub fn get_string_attr(attrs: &[KeyValue], key: &str) -> Option<String> {
    attrs.iter().find(|kv| kv.key == key).and_then(|kv| {
        kv.value.as_ref().and_then(|v| match &v.value {
            Some(any_value::Value::StringValue(s)) => Some(s.clone()),
            _ => None,
        })
    })
}

/// Convert an `AnyValue` to `serde_json::Value`.
pub fn any_value_to_json(val: &AnyValue) -> serde_json::Value {
    match &val.value {
        Some(any_value::Value::StringValue(s)) => serde_json::Value::String(s.clone()),
        Some(any_value::Value::BoolValue(b)) => serde_json::Value::Bool(*b),
        Some(any_value::Value::IntValue(i)) => serde_json::json!(i),
        Some(any_value::Value::DoubleValue(d)) => serde_json::json!(d),
        Some(any_value::Value::ArrayValue(arr)) => {
            serde_json::Value::Array(arr.values.iter().map(any_value_to_json).collect())
        }
        Some(any_value::Value::KvlistValue(kv)) => {
            let map: serde_json::Map<String, serde_json::Value> = kv
                .values
                .iter()
                .filter_map(|kv| {
                    kv.value
                        .as_ref()
                        .map(|v| (kv.key.clone(), any_value_to_json(v)))
                })
                .collect();
            serde_json::Value::Object(map)
        }
        None => serde_json::Value::Null,
    }
}

/// Convert a slice of `KeyValue` to a JSON object.
pub fn attrs_to_json(attrs: &[KeyValue]) -> serde_json::Value {
    if attrs.is_empty() {
        return serde_json::Value::Null;
    }
    let map: serde_json::Map<String, serde_json::Value> = attrs
        .iter()
        .filter_map(|kv| {
            kv.value
                .as_ref()
                .map(|v| (kv.key.clone(), any_value_to_json(v)))
        })
        .collect();
    serde_json::Value::Object(map)
}

/// Map OTLP `SeverityNumber` to our DB level string.
pub fn severity_to_level(num: i32) -> &'static str {
    match num {
        1..=4 => "trace",
        5..=8 => "debug",
        13..=16 => "warn",
        17..=20 => "error",
        21..=24 => "fatal",
        _ => "info",
    }
}

/// Map OTLP `SpanKind` enum value to DB string.
pub fn span_kind_to_str(kind: i32) -> &'static str {
    match kind {
        2 => "server",
        3 => "client",
        4 => "producer",
        5 => "consumer",
        _ => "internal",
    }
}

/// Map OTLP `StatusCode` to DB string.
pub fn status_code_to_str(code: i32) -> &'static str {
    match code {
        1 => "ok",
        2 => "error",
        _ => "unset",
    }
}

/// Convert nanosecond unix timestamp to `chrono::DateTime<Utc>`.
pub fn nanos_to_datetime(nanos: u64) -> chrono::DateTime<chrono::Utc> {
    #[allow(clippy::cast_possible_wrap)]
    let secs = (nanos / 1_000_000_000) as i64;
    #[allow(clippy::cast_possible_truncation)]
    let nsec = (nanos % 1_000_000_000) as u32;
    chrono::DateTime::from_timestamp(secs, nsec).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_id_hex_roundtrip() {
        let bytes = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        assert_eq!(trace_id_to_hex(&bytes), "0102030405060708090a0b0c0d0e0f10");
    }

    #[test]
    fn span_id_hex_roundtrip() {
        let bytes = [0xABu8, 0xCD, 0xEF, 0x01, 0x23, 0x45, 0x67, 0x89];
        assert_eq!(span_id_to_hex(&bytes), "abcdef0123456789");
    }

    #[test]
    fn severity_mapping() {
        assert_eq!(severity_to_level(0), "info"); // unspecified -> default info
        assert_eq!(severity_to_level(1), "trace");
        assert_eq!(severity_to_level(5), "debug");
        assert_eq!(severity_to_level(9), "info");
        assert_eq!(severity_to_level(13), "warn");
        assert_eq!(severity_to_level(17), "error");
        assert_eq!(severity_to_level(21), "fatal");
    }

    #[test]
    fn span_kind_mapping() {
        assert_eq!(span_kind_to_str(0), "internal");
        assert_eq!(span_kind_to_str(1), "internal");
        assert_eq!(span_kind_to_str(2), "server");
        assert_eq!(span_kind_to_str(3), "client");
        assert_eq!(span_kind_to_str(4), "producer");
        assert_eq!(span_kind_to_str(5), "consumer");
    }

    #[test]
    fn status_code_mapping() {
        assert_eq!(status_code_to_str(0), "unset");
        assert_eq!(status_code_to_str(1), "ok");
        assert_eq!(status_code_to_str(2), "error");
    }

    #[test]
    fn get_string_attr_found() {
        let attrs = vec![KeyValue {
            key: "service.name".into(),
            value: Some(AnyValue {
                value: Some(any_value::Value::StringValue("my-svc".into())),
            }),
        }];
        assert_eq!(
            get_string_attr(&attrs, "service.name"),
            Some("my-svc".into())
        );
    }

    #[test]
    fn get_string_attr_missing() {
        let attrs: Vec<KeyValue> = vec![];
        assert_eq!(get_string_attr(&attrs, "nope"), None);
    }

    #[test]
    fn attrs_to_json_empty() {
        assert_eq!(attrs_to_json(&[]), serde_json::Value::Null);
    }

    #[test]
    fn attrs_to_json_string_and_int() {
        let attrs = vec![
            KeyValue {
                key: "method".into(),
                value: Some(AnyValue {
                    value: Some(any_value::Value::StringValue("GET".into())),
                }),
            },
            KeyValue {
                key: "status".into(),
                value: Some(AnyValue {
                    value: Some(any_value::Value::IntValue(200)),
                }),
            },
        ];
        let json = attrs_to_json(&attrs);
        assert_eq!(json["method"], "GET");
        assert_eq!(json["status"], 200);
    }

    #[test]
    fn nanos_to_datetime_works() {
        let dt = nanos_to_datetime(1_700_000_000_000_000_000);
        assert_eq!(dt.timestamp(), 1_700_000_000);
    }

    #[test]
    fn encode_decode_trace_request() {
        let req = ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                resource: Some(Resource {
                    attributes: vec![KeyValue {
                        key: "service.name".into(),
                        value: Some(AnyValue {
                            value: Some(any_value::Value::StringValue("test".into())),
                        }),
                    }],
                }),
                scope_spans: vec![ScopeSpans {
                    scope: None,
                    spans: vec![Span {
                        trace_id: vec![1; 16],
                        span_id: vec![2; 8],
                        parent_span_id: vec![],
                        name: "root".into(),
                        kind: SpanKind::Server as i32,
                        start_time_unix_nano: 1_000_000_000,
                        end_time_unix_nano: 2_000_000_000,
                        attributes: vec![],
                        events: vec![],
                        status: Some(SpanStatus {
                            message: String::new(),
                            code: StatusCode::Ok as i32,
                        }),
                    }],
                }],
            }],
        };

        let bytes = req.encode_to_vec();
        let decoded = ExportTraceServiceRequest::decode(bytes.as_slice()).unwrap();
        assert_eq!(decoded.resource_spans.len(), 1);
        assert_eq!(
            decoded.resource_spans[0].scope_spans[0].spans[0].name,
            "root"
        );
    }
}
