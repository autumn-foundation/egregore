#![allow(missing_docs)]
//! Attribute/macro route registration visibility (issue #445).
//!
//! `eg scan` must (1) emit a `REGISTERS_ROUTE` edge from the symbol owning a
//! route-registration macro invocation (`routes![handler_a, handler_b]`) to each
//! registered handler `Symbol`, and (2) capture routing attributes
//! (`#[get("/path")]`, `#[post("/path")]`, …) as a queryable `route` fact stored
//! on the handler `Symbol` node, so route→handler chains are traceable and
//! attribute-routed handlers are not misclassified as unreferenced / test-only.
//!
//! These assertions operate on the emitted JSONL string values, so they COMPILE
//! without the new `EdgeLabel::REGISTERS_ROUTE` variant or the new Symbol `route`
//! field existing yet — they are RED against current trunk (0 registration edges,
//! unstored route attributes) and turn GREEN once the extractor + resolver land.

use std::{fs, path::Path};

use aletheia_egregore::scan_repository_at_with_override;
use serde_json::Value;

const FIXED_TIME: &str = "2026-06-07T00:00:00Z";
const REPO_ID: &str = "route-registration-fixture";

fn write_fixture(root: &Path, files: &[(&str, &str)]) {
    for (relative, contents) in files {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("fixture file should have a parent"))
            .expect("fixture parent dir should be created");
        fs::write(path, contents).expect("fixture file should be written");
    }
}

fn scan_jsonl(root: &Path) -> String {
    scan_repository_at_with_override(root, FIXED_TIME, Some(REPO_ID))
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize")
}

fn scan_fixture(root: &Path) -> Vec<Value> {
    parse_jsonl(&scan_jsonl(root))
}

fn parse_jsonl(jsonl: &str) -> Vec<Value> {
    jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("record should be valid JSON"))
        .collect()
}

/// A handler `Symbol` is identified by its file path and the LAST segment of its
/// (possibly module-qualified) name, so the lookup does not depend on the exact
/// module-qualification scheme (e.g. `handlers::list_contacts` vs `list_contacts`).
fn name_matches(full: &str, simple: &str) -> bool {
    full == simple || full.rsplit("::").next() == Some(simple)
}

fn symbol_node<'a>(
    records: &'a [Value],
    symbol_kind: &str,
    simple_name: &str,
    path: &str,
) -> &'a Value {
    records
        .iter()
        .find(|record| {
            record["record_type"] == "node"
                && record["kind"] == "Symbol"
                && record["symbol_kind"] == symbol_kind
                && record["repo_relative_path"] == path
                && name_matches(record["name"].as_str().unwrap_or_default(), simple_name)
        })
        .unwrap_or_else(|| panic!("missing {symbol_kind} symbol {simple_name} in {path}"))
}

fn symbol_id(records: &[Value], symbol_kind: &str, simple_name: &str, path: &str) -> String {
    symbol_node(records, symbol_kind, simple_name, path)["id"]
        .as_str()
        .expect("symbol should have an ID")
        .to_owned()
}

fn registers_route_edge<'a>(records: &'a [Value], source: &str, target: &str) -> Option<&'a Value> {
    records.iter().find(|record| {
        record["record_type"] == "edge"
            && record["label"] == "REGISTERS_ROUTE"
            && record["source"] == source
            && record["target"] == target
    })
}

fn registers_route_edges_targeting<'a>(records: &'a [Value], target: &str) -> Vec<&'a Value> {
    records
        .iter()
        .filter(|record| {
            record["record_type"] == "edge"
                && record["label"] == "REGISTERS_ROUTE"
                && record["target"] == target
        })
        .collect()
}

/// A crate that annotates handlers with routing attributes and registers them
/// through a `routes![…]` macro. `list_contacts`'s ONLY non-registration caller
/// is a `#[test]` fn (the dead-code-honesty case), so without a registration edge
/// it reads as test-only. `create_contact` exercises a `#[post(...)]`-flavored
/// attribute alongside the Rocket-style `#[get(...)]` handlers.
fn route_fixture() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "src/handlers.rs",
            concat!(
                "#[get(\"/api/v1/contacts\")]\n",
                "pub fn list_contacts() -> String {\n",
                "    String::new()\n",
                "}\n\n",
                "#[get(\"/api/v1/contacts/{id}\")]\n",
                "pub fn get_contact() -> String {\n",
                "    String::new()\n",
                "}\n\n",
                "#[post(\"/api/v1/contacts\")]\n",
                "pub fn create_contact() -> String {\n",
                "    String::new()\n",
                "}\n",
            ),
        ),
        (
            "src/app.rs",
            concat!(
                "use crate::handlers::{create_contact, get_contact, list_contacts};\n\n",
                "pub fn build() {\n",
                "    let _routes = routes![list_contacts, get_contact, create_contact];\n",
                "}\n\n",
                "#[cfg(test)]\n",
                "mod tests {\n",
                "    use super::*;\n\n",
                "    #[test]\n",
                "    fn only_a_test_calls_list_contacts() {\n",
                "        let _ = list_contacts();\n",
                "    }\n",
                "}\n",
            ),
        ),
    ]
}

// ── (b) RED assertion 1: registration edge routes![…] → each handler ──────────

#[test]
fn routes_macro_emits_registers_route_edge_to_each_handler() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(repo, &route_fixture());

    let records = scan_fixture(repo);
    let build = symbol_id(&records, "function", "build", "src/app.rs");
    let list = symbol_id(&records, "function", "list_contacts", "src/handlers.rs");
    let get = symbol_id(&records, "function", "get_contact", "src/handlers.rs");
    let create = symbol_id(&records, "function", "create_contact", "src/handlers.rs");

    for (handler_id, handler) in [
        (&list, "list_contacts"),
        (&get, "get_contact"),
        (&create, "create_contact"),
    ] {
        assert!(
            registers_route_edge(&records, &build, handler_id).is_some(),
            "routes![…] site {build} must emit a REGISTERS_ROUTE edge to handler {handler} ({handler_id})"
        );
    }
}

// ── (c) RED assertion 2: route attribute stored on the handler Symbol node ────

#[test]
fn handler_symbol_carries_route_attribute_fact() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(repo, &route_fixture());

    let records = scan_fixture(repo);
    let list = symbol_node(&records, "function", "list_contacts", "src/handlers.rs");
    let route = &list["route"];

    assert!(
        !route.is_null(),
        "handler Symbol must carry a `route` fact (method + path); got none: {list}"
    );
    // The route fact must encode the HTTP method and path regardless of the exact
    // shape chosen (struct / vec-of-struct / object).
    let route_text = route.to_string();
    assert!(
        route_text.contains("GET"),
        "route fact on list_contacts must encode method GET: {route}"
    );
    assert!(
        route_text.contains("/api/v1/contacts"),
        "route fact on list_contacts must encode path /api/v1/contacts: {route}"
    );
}

// ── (d) RED assertion 3: not-dead honesty — non-test inbound reference ────────

#[test]
fn attribute_routed_handler_gains_non_test_inbound_reference() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(repo, &route_fixture());

    let records = scan_fixture(repo);
    let list = symbol_id(&records, "function", "list_contacts", "src/handlers.rs");

    // Today `list_contacts`'s only inbound reference is a CALLS edge from a
    // `#[test]` fn, so `unreferenced`/dead-code triage reads it as test-only.
    // Once the registration edge exists it has a non-test inbound reference:
    // an inbound REGISTERS_ROUTE edge from the `routes![…]` site.
    let inbound = registers_route_edges_targeting(&records, &list);
    assert!(
        !inbound.is_empty(),
        "list_contacts must have an inbound REGISTERS_ROUTE reference so it is not \
         misclassified as test-only / unreferenced (had {} today)",
        inbound.len()
    );
}

// ── (e) determinism stub (may pass today; kept) ───────────────────────────────

#[test]
fn route_registration_scan_is_byte_deterministic() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(repo, &route_fixture());

    let first = scan_jsonl(repo);
    let second = scan_jsonl(repo);
    assert_eq!(first, second, "repeated scans must be byte-identical");
}
