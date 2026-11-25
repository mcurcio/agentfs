//! Tests only: the one in-memory reader of OpenTelemetry metrics that the
//! engines' tests share, so neither engine's tests depend on the other
//! (docs/telemetry.md lists the metrics). [`MetricReader`] makes a meter
//! over the SDK's [`InMemoryMetricExporter`] and answers the point of a
//! metric by name and exact attributes.

use opentelemetry::metrics::{Meter, MeterProvider as _};
use opentelemetry::{InstrumentationScope, KeyValue};
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

/// One metric data point, flattened.
#[derive(Debug, Clone, PartialEq)]
pub enum Point {
    U64(u64),
    F64(f64),
    I64(i64),
    /// A histogram's count.
    Count(u64),
}

/// A meter provider whose metrics are read in memory, at each call.
pub struct MetricReader {
    meters: SdkMeterProvider,
    metrics: InMemoryMetricExporter,
}

impl Default for MetricReader {
    fn default() -> Self {
        Self::new()
    }
}

impl MetricReader {
    pub fn new() -> Self {
        let metrics = InMemoryMetricExporter::default();
        let meters = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(metrics.clone()).build())
            .build();
        Self { meters, metrics }
    }

    /// A meter under `scope`, read by this reader.
    pub fn meter(&self, scope: InstrumentationScope) -> Meter {
        #[expect(
            clippy::disallowed_methods,
            reason = "the tests' one meter over the in-memory exporter"
        )]
        self.meters.meter_with_scope(scope)
    }

    /// Collects metrics now and returns the export (cumulative), or `None`
    /// if there was nothing to export.
    ///
    /// # Panics
    ///
    /// The provider could not flush, or the exporter could not be read.
    fn collect(&self) -> Option<ResourceMetrics> {
        self.metrics.reset();
        self.meters.force_flush().unwrap();
        self.metrics.get_finished_metrics().unwrap().pop()
    }

    /// The data point of metric `name` whose attributes are exactly
    /// `attrs`, from a fresh collection.
    pub fn point(&self, name: &str, attrs: &[(&str, &str)]) -> Option<Point> {
        let collected = self.collect()?;
        find_point(&collected, name, attrs)
    }

    /// The `u64` value of a sum or gauge.
    ///
    /// # Panics
    ///
    /// The point is of another kind.
    pub fn sum(&self, name: &str, attrs: &[(&str, &str)]) -> Option<u64> {
        match self.point(name, attrs)? {
            Point::U64(v) => Some(v),
            other => panic!("{name} is {other:?}"),
        }
    }

    /// The `u64` value of a gauge (or sum).
    ///
    /// # Panics
    ///
    /// The point is of another kind.
    pub fn gauge_u64(&self, name: &str, attrs: &[(&str, &str)]) -> Option<u64> {
        self.sum(name, attrs)
    }

    /// The `f64` value of a gauge (or sum).
    ///
    /// # Panics
    ///
    /// The point is of another kind.
    pub fn gauge_f64(&self, name: &str, attrs: &[(&str, &str)]) -> Option<f64> {
        match self.point(name, attrs)? {
            Point::F64(v) => Some(v),
            other => panic!("{name} is {other:?}"),
        }
    }

    /// A histogram's count.
    ///
    /// # Panics
    ///
    /// The point is of another kind.
    pub fn histogram_count(&self, name: &str, attrs: &[(&str, &str)]) -> Option<u64> {
        match self.point(name, attrs)? {
            Point::Count(v) => Some(v),
            other => panic!("{name} is {other:?}"),
        }
    }
}

fn attrs_match<'a>(got: impl Iterator<Item = &'a KeyValue>, want: &[(&str, &str)]) -> bool {
    let mut got: Vec<(String, String)> = got
        .map(|kv| (kv.key.to_string(), kv.value.to_string()))
        .collect();
    got.sort();
    let mut want: Vec<(String, String)> = want
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    want.sort();
    got == want
}

fn find_point(collected: &ResourceMetrics, name: &str, attrs: &[(&str, &str)]) -> Option<Point> {
    let metric = collected
        .scope_metrics()
        .flat_map(|s| s.metrics())
        .find(|m| m.name() == name)?;
    macro_rules! scan {
        ($data:expr, $wrap:path) => {
            match $data {
                MetricData::Sum(sum) => sum
                    .data_points()
                    .find(|p| attrs_match(p.attributes(), attrs))
                    .map(|p| $wrap(p.value())),
                MetricData::Gauge(gauge) => gauge
                    .data_points()
                    .find(|p| attrs_match(p.attributes(), attrs))
                    .map(|p| $wrap(p.value())),
                MetricData::Histogram(h) => h
                    .data_points()
                    .find(|p| attrs_match(p.attributes(), attrs))
                    .map(|p| Point::Count(p.count())),
                MetricData::ExponentialHistogram(_) => None,
            }
        };
    }
    match metric.data() {
        AggregatedMetrics::U64(data) => scan!(data, Point::U64),
        AggregatedMetrics::F64(data) => scan!(data, Point::F64),
        AggregatedMetrics::I64(data) => scan!(data, Point::I64),
    }
}
