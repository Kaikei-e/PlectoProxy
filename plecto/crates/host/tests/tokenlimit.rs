//! Tier A conformance for the `filter-tokenlimit` example guests (JS + MoonBit).
//!
//! Where `tests/polyglot.rs` proves the CONTRACT is language-neutral against the filter-hello
//! conformance subset, this suite proves a real POLICY is: one behaviour spec — price a request
//! from its JSON body, spend that price against the host bucket, report what it cost — implemented
//! independently in two languages and held to byte-identical answers. The assertions live in
//! `tokenlimit_battery` so the Tier B guest (`tests/tokenlimit_tier_b.rs`) drives exactly the same
//! ones; the only thing that differs is the grant each tier's `LoadOptions` needs, which for Tier A
//! is nothing at all — these components arrive zero-WASI and link against the unchanged
//! deny-by-default Linker (ADR 000055).
//!
//! Gated behind `polyglot-conformance` so a plain `cargo test` never needs the non-Rust toolchains.
//! Component paths default to each example's `dist/`; override with
//! `PLECTO_TOKENLIMIT_COMPONENTS=/path/a.wasm:/path/b.wasm`.
#![cfg(feature = "polyglot-conformance")]

mod tokenlimit_battery;

use plecto_host::LoadOptions;
use tokenlimit_battery::{Grant, Guest};

const DEFAULT_COMPONENTS: &[&str] = &[
    "../../examples/filters/filter-tokenlimit-js/dist/filter_tokenlimit_js.wasm",
    "../../examples/filters/filter-tokenlimit-moonbit/dist/filter_tokenlimit_moonbit.wasm",
];

fn guests() -> Vec<Guest> {
    tokenlimit_battery::guests(
        "PLECTO_TOKENLIMIT_COMPONENTS",
        DEFAULT_COMPONENTS,
        "examples/filters/filter-tokenlimit-{js,moonbit}/build.sh",
    )
}

/// Tier A takes no grant: a zero-WASI guest gets the default Linker and nothing more.
const TIER_A: Grant = |opts: LoadOptions| opts;

#[test]
fn every_language_binds_the_040_rail_with_the_request_body_hook_only() {
    for guest in guests() {
        tokenlimit_battery::binds_the_040_rail_with_the_request_body_hook_only(&guest, TIER_A);
    }
}

#[test]
fn every_language_rejects_a_request_without_the_key_header() {
    for guest in guests() {
        tokenlimit_battery::a_request_without_the_key_header_is_401(&guest, TIER_A);
    }
}

#[test]
fn every_language_rejects_a_body_that_is_not_json() {
    for guest in guests() {
        tokenlimit_battery::a_body_that_is_not_json_is_400(&guest, TIER_A);
    }
}

#[test]
fn every_language_charges_and_reports_a_request_within_budget() {
    for guest in guests() {
        tokenlimit_battery::a_request_within_budget_is_charged_and_reported(&guest, TIER_A);
    }
}

#[test]
fn every_language_answers_429_once_the_bucket_is_drained() {
    for guest in guests() {
        tokenlimit_battery::draining_the_bucket_is_429_with_a_retry_after(&guest, TIER_A);
    }
}

#[test]
fn every_language_applies_the_model_cost_multiplier() {
    for guest in guests() {
        tokenlimit_battery::the_model_multiplier_changes_the_charged_cost(&guest, TIER_A);
    }
}

#[test]
fn every_language_keeps_the_scratch_clean_across_a_pooled_401() {
    for guest in guests() {
        tokenlimit_battery::an_unkeyed_request_between_two_charges_does_not_corrupt_the_scratch(
            &guest, TIER_A,
        );
    }
}

#[test]
fn every_language_fails_the_load_on_a_broken_chars_per_token() {
    for guest in guests() {
        tokenlimit_battery::a_broken_chars_per_token_fails_the_load(&guest, TIER_A);
    }
}

#[test]
fn every_language_fails_the_load_on_an_empty_key_header() {
    for guest in guests() {
        tokenlimit_battery::an_empty_key_header_fails_the_load(&guest, TIER_A);
    }
}

#[test]
fn every_language_prices_a_malformed_max_tokens_as_absent() {
    for guest in guests() {
        tokenlimit_battery::a_malformed_max_tokens_prices_as_absent(&guest, TIER_A);
    }
}

#[test]
fn every_language_rejects_a_key_header_that_is_not_utf8() {
    for guest in guests() {
        tokenlimit_battery::a_key_header_that_is_not_utf8_is_401_and_non_ascii_is_accepted(
            &guest, TIER_A,
        );
    }
}

#[test]
fn every_language_skips_the_lookup_for_an_over_long_model_name() {
    for guest in guests() {
        tokenlimit_battery::an_over_long_model_name_is_never_looked_up(&guest, TIER_A);
    }
}

#[test]
fn every_language_fails_the_request_on_an_unparseable_model_multiplier() {
    for guest in guests() {
        tokenlimit_battery::an_unparseable_model_multiplier_fails_the_request_closed(
            &guest, TIER_A,
        );
    }
}
