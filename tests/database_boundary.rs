//! Static regression checks for the customer-plane/shared-domain boundary.

use std::{fs, path::PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(relative: &str) -> String {
    fs::read_to_string(root().join(relative))
        .unwrap_or_else(|error| panic!("failed to read {relative}: {error}"))
}

#[test]
fn package_declares_the_shared_fiducia_library() {
    let manifest = read(".zpkg.toml");
    for contract in [
        "org = \"fiducia-cloud\"",
        "name = \"fiducia-customer\"",
        "\"fiducia-cloud/fiducia-lib\" = \"^0.1.0\"",
        "dir = \".vendor/.zed\"",
    ] {
        assert!(manifest.contains(contract), "zed package contract lost {contract}");
    }
}

#[test]
fn customer_writer_and_shared_reader_are_distinct_contracts() {
    let boundary = read("docs/database-boundary.md");
    for contract in [
        "combined BFF/API deployable",
        "FIDUCIA_SHARED_READ_DATABASE_URL",
        "never reuse `DATABASE_URL`",
        "DbRole::ReadOnly",
        "assert_read_only",
        "queries::read",
        "fiducia-auth",
        "expand → backfill → contract",
        "specialized Fiducia Kubernetes cluster",
    ] {
        assert!(
            boundary.contains(contract),
            "database boundary lost {contract}"
        );
    }

    assert!(
        !boundary.contains("fiducia_orm::queries::write` for coordination-domain data is allowed"),
        "shared-domain writes must not be granted to the customer tier"
    );
}

#[test]
fn current_writable_connection_remains_scoped_to_customer_state() {
    let main = read("src/main.rs");
    for contract in [
        "connect_customer_db()",
        "FIDUCIA_DB_SCHEMA",
        "unwrap_or_else(|| \"fiducia\".to_string())",
        "API-key lifecycle",
        "delegated to fiducia-auth",
    ] {
        assert!(main.contains(contract), "customer composition lost {contract}");
    }

    let store = read("src/store.rs");
    for customer_owned_operation in [
        "ensure_user",
        "upsert_preferences",
        "revoke_session",
        "mark_notification_read",
    ] {
        assert!(
            store.contains(customer_owned_operation),
            "customer-owned store lost {customer_owned_operation}"
        );
    }

    for forbidden_shared_seam in [
        "FIDUCIA_SHARED_READ_DATABASE_URL",
        "fiducia_orm::queries::write",
        "DbRole::ReadWrite",
    ] {
        assert!(
            !main.contains(forbidden_shared_seam) && !store.contains(forbidden_shared_seam),
            "customer writer crossed into shared-domain seam {forbidden_shared_seam}"
        );
    }
}

#[test]
fn seaorm_remains_the_only_direct_database_layer() {
    let manifest = read("Cargo.toml");
    assert!(manifest.contains("sea-orm ="));
    for direct_dependency in ["sqlx =", "tokio-postgres =", "bb8-postgres ="] {
        assert!(
            !manifest
                .lines()
                .any(|line| line.trim_start().starts_with(direct_dependency)),
            "forbidden parallel data layer {direct_dependency}"
        );
    }
}
