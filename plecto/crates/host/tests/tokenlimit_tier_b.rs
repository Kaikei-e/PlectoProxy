//! Tier B conformance for `filter-tokenlimit-go` — the token-cost limiter compiled with TinyGo,
//! held to the SAME assertion battery as the Tier A guests (`tests/tokenlimit.rs`).
//!
//! Kept in its own file for the reason `tests/polyglot_tier_b.rs` states (ADR 000063): a fat guest
//! needs `LoadOptions::with_wasi_minimal()`, and folding it into the Tier A loop would either break
//! those options or hand every zero-WASI guest a grant it must not have — diluting exactly the claim
//! the Tier A loop exists to prove (ADR 000055). Everything else is shared: the two files differ
//! only in which components they load and which [`Grant`] they apply.
//!
//! Gated behind BOTH `polyglot-conformance` (so a plain `cargo test` never needs TinyGo) AND
//! `fat-guest` (so it never needs `with_wasi_minimal`, which does not exist otherwise). Override the
//! component path with `PLECTO_TOKENLIMIT_GO_COMPONENT`.
#![cfg(all(feature = "polyglot-conformance", feature = "fat-guest"))]

mod tokenlimit_battery;

use plecto_host::LoadOptions;
use tokenlimit_battery::{Grant, Guest};

const DEFAULT_COMPONENT: &[&str] =
    &["../../examples/filters/filter-tokenlimit-go/dist/filter_tokenlimit_go.wasm"];

fn guests() -> Vec<Guest> {
    tokenlimit_battery::guests(
        "PLECTO_TOKENLIMIT_GO_COMPONENT",
        DEFAULT_COMPONENT,
        "examples/filters/filter-tokenlimit-go/build.sh",
    )
}

/// Tier B's one difference from Tier A: the fixed minimal-WASI slice a TinyGo runtime needs to boot
/// at all (ADR 000063). It is a grant, not a default — see the deny-by-default assertion below.
const TIER_B: Grant = |opts: LoadOptions| opts.with_wasi_minimal();

/// The Tier A grant — no grant at all. Used only to prove this guest cannot load with it.
const NO_GRANT: Grant = |opts: LoadOptions| opts;

#[test]
fn without_the_wasi_minimal_grant_the_fat_guest_fails_to_link() {
    // The tier boundary itself, asserted before anything else: this guest's unresolved
    // wasi:cli/wasi:filesystem imports must fail instantiation structurally when the manifest never
    // declared `wasi = "minimal"` — a policy filter is not exempt from deny-by-default. Everything
    // else about the load is identical to the passing cases, so the grant is the only variable.
    for guest in guests() {
        let opts = tokenlimit_battery::options(NO_GRANT, 1_000, tokenlimit_battery::base_config());
        let err = tokenlimit_battery::try_load(&guest, opts)
            .err()
            .expect("a fat guest without the wasi_minimal grant must fail to load");
        let msg = err.to_string().to_lowercase();
        assert!(
            msg.contains("import") || msg.contains("wasi") || msg.contains("unknown"),
            "expected an unresolved-import style failure, got: {err}"
        );
    }
}

#[test]
fn the_fat_guest_binds_the_040_rail_with_the_request_body_hook_only() {
    for guest in guests() {
        tokenlimit_battery::binds_the_040_rail_with_the_request_body_hook_only(&guest, TIER_B);
    }
}

#[test]
fn the_fat_guest_rejects_a_request_without_the_key_header() {
    for guest in guests() {
        tokenlimit_battery::a_request_without_the_key_header_is_401(&guest, TIER_B);
    }
}

#[test]
fn the_fat_guest_rejects_a_body_that_is_not_json() {
    for guest in guests() {
        tokenlimit_battery::a_body_that_is_not_json_is_400(&guest, TIER_B);
    }
}

#[test]
fn the_fat_guest_charges_and_reports_a_request_within_budget() {
    for guest in guests() {
        tokenlimit_battery::a_request_within_budget_is_charged_and_reported(&guest, TIER_B);
    }
}

#[test]
fn the_fat_guest_answers_429_once_the_bucket_is_drained() {
    for guest in guests() {
        tokenlimit_battery::draining_the_bucket_is_429_with_a_retry_after(&guest, TIER_B);
    }
}

#[test]
fn the_fat_guest_applies_the_model_cost_multiplier() {
    for guest in guests() {
        tokenlimit_battery::the_model_multiplier_changes_the_charged_cost(&guest, TIER_B);
    }
}

#[test]
fn the_fat_guest_keeps_the_scratch_clean_across_a_pooled_401() {
    for guest in guests() {
        tokenlimit_battery::an_unkeyed_request_between_two_charges_does_not_corrupt_the_scratch(
            &guest, TIER_B,
        );
    }
}

#[test]
fn the_fat_guest_fails_the_load_on_a_broken_chars_per_token() {
    for guest in guests() {
        tokenlimit_battery::a_broken_chars_per_token_fails_the_load(&guest, TIER_B);
    }
}

#[test]
fn the_fat_guest_fails_the_load_on_an_empty_key_header() {
    for guest in guests() {
        tokenlimit_battery::an_empty_key_header_fails_the_load(&guest, TIER_B);
    }
}

#[test]
fn the_fat_guest_prices_a_malformed_max_tokens_as_absent() {
    for guest in guests() {
        tokenlimit_battery::a_malformed_max_tokens_prices_as_absent(&guest, TIER_B);
    }
}

#[test]
fn the_fat_guest_rejects_a_key_header_that_is_not_utf8() {
    for guest in guests() {
        tokenlimit_battery::a_key_header_that_is_not_utf8_is_401_and_non_ascii_is_accepted(
            &guest, TIER_B,
        );
    }
}

#[test]
fn the_fat_guest_skips_the_lookup_for_an_over_long_model_name() {
    for guest in guests() {
        tokenlimit_battery::an_over_long_model_name_is_never_looked_up(&guest, TIER_B);
    }
}

#[test]
fn the_fat_guest_fails_the_request_on_an_unparseable_model_multiplier() {
    for guest in guests() {
        tokenlimit_battery::an_unparseable_model_multiplier_fails_the_request_closed(
            &guest, TIER_B,
        );
    }
}
