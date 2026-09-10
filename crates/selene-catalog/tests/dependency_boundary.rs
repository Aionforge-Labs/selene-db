//! Workspace dependency-direction assertion for the catalog boundary.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    process::Command,
};

use serde_json::Value;

#[test]
fn catalog_dependencies_are_allowlisted_and_workspace_is_acyclic() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("catalog crate is two levels below the workspace root");
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--locked", "--format-version", "1", "--no-deps"])
        .current_dir(workspace_root)
        .output()
        .expect("cargo metadata executes");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let metadata: Value = serde_json::from_slice(&output.stdout).expect("metadata is JSON");
    let packages = metadata["packages"]
        .as_array()
        .expect("metadata packages is an array");
    let workspace_packages = packages
        .iter()
        .filter_map(|package| package["name"].as_str())
        .filter(|name| name.starts_with("selene-db"))
        .collect::<BTreeSet<_>>();

    let catalog = packages
        .iter()
        .find(|package| package["name"] == "selene-db-catalog")
        .expect("catalog package is in workspace metadata");
    let direct = normal_workspace_dependencies(catalog, &workspace_packages);
    assert_eq!(
        direct,
        BTreeSet::from(["selene-db-core", "selene-db-profile"]),
        "catalog may consume only storage-neutral core/profile declarations"
    );

    let graph = packages
        .iter()
        .filter_map(|package| {
            let name = package["name"].as_str()?;
            workspace_packages.contains(name).then(|| {
                (
                    name,
                    normal_workspace_dependencies(package, &workspace_packages),
                )
            })
        })
        .collect::<BTreeMap<_, _>>();
    assert_acyclic(&graph);
}

fn normal_workspace_dependencies<'a>(
    package: &'a Value,
    workspace_packages: &BTreeSet<&'a str>,
) -> BTreeSet<&'a str> {
    package["dependencies"]
        .as_array()
        .expect("package dependencies is an array")
        .iter()
        .filter(|dependency| dependency["kind"].is_null())
        .filter_map(|dependency| dependency["name"].as_str())
        .filter(|name| workspace_packages.contains(name))
        .collect()
}

fn assert_acyclic<'a>(graph: &BTreeMap<&'a str, BTreeSet<&'a str>>) {
    fn visit<'a>(
        node: &'a str,
        graph: &BTreeMap<&'a str, BTreeSet<&'a str>>,
        visiting: &mut BTreeSet<&'a str>,
        visited: &mut BTreeSet<&'a str>,
    ) {
        assert!(
            visiting.insert(node),
            "workspace dependency cycle at {node}"
        );
        if let Some(dependencies) = graph.get(node) {
            for dependency in dependencies {
                if !visited.contains(dependency) {
                    visit(dependency, graph, visiting, visited);
                }
            }
        }
        visiting.remove(node);
        visited.insert(node);
    }

    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    for node in graph.keys() {
        if !visited.contains(node) {
            visit(node, graph, &mut visiting, &mut visited);
        }
    }
}
