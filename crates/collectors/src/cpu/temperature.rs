//! The CPU package temperature of GB10 (D-68). Its firmware reports the E
//! and P cores of its two clusters but no package: the package is the
//! hottest of the four, derived once all four are selected and aligned in
//! time (`FORMULA_GB10_CLUSTER_MAX`, every input listed), else unavailable.
//! Where a sensor reports the package itself, it is published as measured
//! and nothing is derived.

use std::collections::BTreeSet;

use dgxtop_core::identity::{MetricKey, Scope};
use dgxtop_core::metric::{Scalar, ids};
use dgxtop_core::naming::entity;

use crate::arbitration::Selection;
use crate::context::Session;
use crate::formulas::{self, Selections};
use crate::reasons;
use crate::values::{self, Evidence};

/// GB10's cluster sensors of one package, as the thermal profile scopes
/// them.
const GB10_CLUSTERS: [&str; 4] = ["cluster:0e", "cluster:0p", "cluster:1e", "cluster:1p"];

/// Publishes the package temperature of every package whose cluster
/// sensors have new evidence.
pub(super) fn publish(session: &mut Session<'_>, selections: &mut Selections) {
    let packages: BTreeSet<MetricKey> = session
        .keys()
        .into_iter()
        .filter(|key| key.metric_id.as_str() == ids::CPU_TEMPERATURE_CLUSTER)
        .filter_map(package_key)
        .collect();
    for package in packages {
        if !session.candidates(&package).is_empty() {
            continue;
        }
        let clusters: Vec<MetricKey> = GB10_CLUSTERS
            .iter()
            .filter_map(|scope| cluster_key(&package, scope))
            .collect();
        let inputs: Vec<&MetricKey> = clusters.iter().collect();
        let triggered = inputs
            .iter()
            .any(|key| session.has_new(key) || session.is_expiring(key));
        if !triggered {
            continue;
        }
        let result = formulas::aligned_inputs(session, &inputs, selections).and_then(hottest);
        publish_package(session, package, &inputs, result);
    }
}

/// The hottest of `inputs`, with their evidence.
fn hottest(inputs: Vec<Selection>) -> Result<(Scalar, Vec<Evidence>), &'static str> {
    let celsius = inputs
        .iter()
        .map(|input| input.value.to_f64_lossy())
        .fold(f64::NEG_INFINITY, f64::max);
    let value = Scalar::from_f64(celsius).map_err(|_| reasons::MISSING_INPUT)?;
    Ok((value, inputs.iter().map(Evidence::of_selection).collect()))
}

fn publish_package(
    session: &mut Session<'_>,
    key: MetricKey,
    inputs: &[&MetricKey],
    result: Result<(Scalar, Vec<Evidence>), &'static str>,
) {
    let context = session.context();
    let value = match result {
        Ok((value, evidence)) => values::derived(
            context,
            key,
            value,
            &evidence,
            &[reasons::FORMULA_GB10_CLUSTER_MAX],
        ),
        Err(code) => {
            let (evidence, freshness) = formulas::input_evidence(session, inputs);
            values::unavailable(
                context,
                key,
                &evidence,
                &[reasons::FORMULA_GB10_CLUSTER_MAX, code],
                freshness,
            )
        }
    };
    if let Some(value) = value {
        session.publish_value(value);
    }
}

/// A cluster sensor's package: `(cpu.package.N, cpu.temperature.package,
/// package:N)`.
fn package_key(cluster: &MetricKey) -> Option<MetricKey> {
    let index: u32 = cluster
        .entity_id
        .as_str()
        .strip_prefix("cpu.package.")?
        .parse()
        .ok()?;
    values::with_series(
        cluster,
        entity::cpu_package(index).ok()?,
        ids::CPU_TEMPERATURE_PACKAGE,
        entity::scope_package(index).ok()?,
    )
    .ok()
}

fn cluster_key(package: &MetricKey, scope: &str) -> Option<MetricKey> {
    values::with_series(
        package,
        package.entity_id.clone(),
        ids::CPU_TEMPERATURE_CLUSTER,
        Scope::new(scope).ok()?,
    )
    .ok()
}
