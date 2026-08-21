//! Static `plecto:filter` contract targeting (ADR 000114): does a component's own type section
//! satisfy any world this build ships?
//!
//! The evidence is `bindgen!`'s `COMPONENT_TYPE` — each shipped world encoded into the binary —
//! so the judgement needs neither the `wit/` directory on disk nor a wasmtime `Engine`, `Store`,
//! or compilation. That is what lets `plecto validate --resolve` run it while keeping the
//! "mutates nothing" contract of ADR 000046 / 000094.
//!
//! The gate only ever ADDS a rejection. A component that satisfies a world may still fail the
//! real load — `wit_component::targets` validates with `WasmFeatures::all()`, looser than the
//! production `Config` (ADR 000113), and the loader picks its adapter by import name
//! (`detect_contract_version`), a mechanism this one deliberately does not share.

use std::collections::BTreeSet;
use std::sync::OnceLock;

use wasmtime::component::wit_parser::{Resolve, WorldId};

use crate::{ContractVersion, LoadError, SUPPORTED_CONTRACT_VERSIONS};

/// Which shipped `plecto:filter` worlds a component statically satisfies, in the verdict
/// vocabulary ADR 000108 took from ISO/IEC 9646: [`Satisfied`](Self::Satisfied) is *pass*, an
/// `Err` is *fail*, and [`Inconclusive`](Self::Inconclusive) is a case this gate must not judge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractTargetVerdict {
    /// The shipped worlds this component targets — never empty.
    Satisfied(Vec<ContractVersion>),
    /// The component imports names no shipped world declares, so world subtyping would reject it
    /// for a reason that is the operator's manifest to settle, not this gate's.
    Inconclusive { imports: Vec<String> },
}

/// Check `component` against every `plecto:filter` world this build ships.
///
/// Two stages. First decidability: a component whose imports reach outside the contract (a Tier B
/// guest's `wasi:*`, an outbound capability) is left [`Inconclusive`](ContractTargetVerdict) —
/// what is lent to it is the operator's decision, and `targets` requires the component's imports
/// to be a subset of the world's. Only a component importing nothing but the contract reaches the
/// second stage, where all four shipped worlds are evaluated — never short-circuited, since the
/// host has no notion of "first match" — and an empty result is the sole rejection.
///
/// Judged against each version's `filter` world, the header-only floor: `filter-body` is a
/// superset of its exports, so a header-only filter and a body filter pass through the same
/// check, and the absence of the body exports keeps the meaning ADR 000038 gave it.
// INVARIANT: `world` is the id `decode` just minted inside `resolve`'s own arena, so indexing
// `resolve.worlds` with it is always in bounds.
#[allow(clippy::indexing_slicing)]
pub fn check_contract_target(component: &[u8]) -> Result<ContractTargetVerdict, LoadError> {
    if !is_component(component) {
        return Err(LoadError::ContractUndecodable(anyhow::anyhow!(
            "not a WebAssembly component: there is no component type section to read a world from"
        )));
    }
    let shipped = shipped_worlds()?;
    let decoded = wit_component::decode(component).map_err(LoadError::ContractUndecodable)?;
    let (resolve, world) = match &decoded {
        wit_component::DecodedWasm::Component(resolve, world) => (resolve, *world),
        wit_component::DecodedWasm::WitPackage(..) => {
            return Err(LoadError::ContractUndecodable(anyhow::anyhow!(
                "the input is an encoded WIT package, not a component"
            )));
        }
    };

    let extra: Vec<String> = resolve.worlds[world]
        .imports
        .keys()
        .map(|key| resolve.name_world_key(key))
        .filter(|name| !shipped.contract_imports.contains(name))
        .collect();
    if !extra.is_empty() {
        return Ok(ContractTargetVerdict::Inconclusive { imports: extra });
    }

    let mut satisfied = Vec::new();
    // Only the newest world's reason is kept: it is the version a new filter aims at, and four
    // subtyping diffs side by side are unreadable. Oldest-first iteration makes the last failure
    // recorded the newest one — and this is read only when nothing satisfied, so the newest did
    // fail.
    let mut newest_failure = None;
    for (&version, (world_resolve, world_id)) in
        SUPPORTED_CONTRACT_VERSIONS.iter().zip(&shipped.worlds)
    {
        match wit_component::targets(world_resolve, *world_id, component) {
            Ok(()) => satisfied.push(version),
            Err(e) => newest_failure = Some(e),
        }
    }
    if satisfied.is_empty() {
        return Err(LoadError::ContractNotSatisfied {
            tried: SUPPORTED_CONTRACT_VERSIONS
                .iter()
                .map(|v| v.package())
                .collect(),
            detail: newest_failure.map_or_else(String::new, |e| format!("{e:#}")),
        });
    }
    Ok(ContractTargetVerdict::Satisfied(satisfied))
}

/// A core module decodes into the same empty world an exportless component does — `decode`
/// collects component import/export sections and a module simply has none — so the container kind
/// has to be read from the preamble: bytes 6..8 are the binary format's `layer`, zero for a core
/// module and non-zero for a component.
fn is_component(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\0asm") && bytes.get(6..8).is_some_and(|layer| layer != [0, 0])
}

/// The shipped worlds, decoded once: `COMPONENT_TYPE` is a constant, so re-decoding it per filter
/// would be pure waste.
struct ShippedWorlds {
    /// One decoded world per [`SUPPORTED_CONTRACT_VERSIONS`] entry, in that order.
    worlds: Vec<(Resolve, WorldId)>,
    /// Every import name any shipped world declares. WIT-level names off the decoded type graph
    /// (`plecto:filter/host-log@0.4.0`), never a byte scan.
    contract_imports: BTreeSet<String>,
}

fn shipped_worlds() -> Result<&'static ShippedWorlds, LoadError> {
    static WORLDS: OnceLock<Result<ShippedWorlds, String>> = OnceLock::new();
    // A failure here means the wasm-tools series that encoded `COMPONENT_TYPE` and the one
    // decoding it drifted apart — a build-integrity fault, not this component's, but still
    // fail-closed. `every_shipped_world_decodes_back_to_its_own_package` pins it in CI.
    WORLDS
        .get_or_init(decode_shipped_worlds)
        .as_ref()
        .map_err(|e| LoadError::ContractUndecodable(anyhow::anyhow!("{e}")))
}

// INVARIANT: same as `check_contract_target` — `decode_world` returns the world id it just made
// inside the `resolve` it returns alongside.
#[allow(clippy::indexing_slicing)]
fn decode_shipped_worlds() -> Result<ShippedWorlds, String> {
    let mut worlds = Vec::with_capacity(SUPPORTED_CONTRACT_VERSIONS.len());
    let mut contract_imports = BTreeSet::new();
    for &version in SUPPORTED_CONTRACT_VERSIONS {
        let (resolve, world) = wasmtime::component::wit_parser::decoding::decode_world(
            crate::contract::component_type(version),
        )
        .map_err(|e| format!("shipped world {}: {e:#}", version.package()))?;
        contract_imports.extend(
            resolve.worlds[world]
                .imports
                .keys()
                .map(|key| resolve.name_world_key(key)),
        );
        worlds.push((resolve, world));
    }
    Ok(ShippedWorlds {
        worlds,
        contract_imports,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The encoder↔decoder pin (ADR 000114): `bindgen!` encodes each shipped world with the
    /// wasm-tools series wasmtime bundles, and this gate decodes it through wasmtime's own
    /// `wit_parser` re-export. If a wasmtime bump ever moves one without the other, the failure
    /// must be this assertion, not a silent misjudgement in CI.
    #[test]
    fn every_shipped_world_decodes_back_to_its_own_package() {
        for &version in SUPPORTED_CONTRACT_VERSIONS {
            let bytes = crate::contract::component_type(version);
            let (resolve, world) = wasmtime::component::wit_parser::decoding::decode_world(bytes)
                .unwrap_or_else(|e| {
                    panic!(
                        "{} COMPONENT_TYPE decodes as a world: {e:#}",
                        version.package()
                    )
                });
            let package = resolve.worlds[world]
                .package
                .expect("a decoded world belongs to a package");
            assert_eq!(
                resolve.packages[package].name.to_string(),
                version.package(),
                "the decoded package names the version that encoded it"
            );
        }
    }

    /// The two mechanisms are independent — the load gate picks an adapter by import name, this
    /// gate checks world subtyping — so they are cross-checked rather than shared. Every in-tree
    /// reference guest must be judged by both, and the static answer must contain the load-time
    /// one.
    #[cfg(feature = "test-support")]
    #[test]
    fn the_static_verdict_contains_the_load_time_contract_version() {
        let engine = crate::engine::build_engine(crate::engine::Allocation::OnDemand).unwrap();
        for bytes in [
            crate::test_support::filter_compat_v01_component(),
            crate::test_support::filter_compat_v02_component(),
            crate::test_support::filter_hello_component(),
            crate::test_support::filter_v04_component(),
        ] {
            let component = wasmtime::component::Component::new(&engine, &bytes).unwrap();
            let detected = crate::contract::detect_contract_version(&component, &engine)
                .expect("a reference guest imports a recognised contract version");
            match check_contract_target(&bytes).unwrap() {
                ContractTargetVerdict::Satisfied(versions) => assert!(
                    versions.contains(&detected),
                    "static {versions:?} contains the load-time answer {detected:?}"
                ),
                other => panic!("a reference guest satisfies a shipped world, got {other:?}"),
            }
        }
    }

    /// A guest that imports outside the contract (Tier B / outbound) is the operator's manifest
    /// to grant, not this gate's to reject — `targets` would refuse it for a reason that is not
    /// the component's fault (ADR 000108's `environment` split).
    #[test]
    fn imports_outside_the_contract_are_left_unjudged() {
        let component = wat::parse_str(
            r#"(component
                 (import "wasi:cli/environment@0.2.6" (instance
                   (export "get-environment" (func (result (list (tuple string string)))))
                 ))
               )"#,
        )
        .unwrap();
        match check_contract_target(&component).unwrap() {
            ContractTargetVerdict::Inconclusive { imports } => assert!(
                imports.iter().any(|i| i == "wasi:cli/environment@0.2.6"),
                "the unjudged imports are named: {imports:?}"
            ),
            other => panic!("a wasi-importing component is inconclusive, got {other:?}"),
        }
    }

    /// Bytes whose type section is not a WIT world cannot satisfy any world, so they are
    /// rejected rather than left unjudged — fail-closed on the same input the real load refuses.
    #[test]
    fn an_undecodable_input_is_rejected() {
        let core_module = wat::parse_str("(module)").unwrap();
        match check_contract_target(&core_module) {
            Err(LoadError::ContractUndecodable(_)) => {}
            other => panic!("a core module is undecodable, got {other:?}"),
        }
    }
}
