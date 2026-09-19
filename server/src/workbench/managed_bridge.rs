//! Managed bridge disk package: manifest, version protection, repair and migration.
//! The controller owns authorization, locking, live validation and activation.

use super::{sha256, WORKBENCH_BRIDGE_PROTOCOL_VERSION, WORKBENCH_BRIDGE_VERSION};
use crate::workbench_bridge::*;
use semver::Version;
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

pub(super) struct Package<'a> {
    directory: &'a Path,
    pub(super) manifest: Option<BridgeManifest>,
}

impl<'a> Package<'a> {
    pub(super) fn read(directory: &'a Path) -> Self {
        Self {
            directory,
            manifest: read_manifest(directory),
        }
    }

    pub(super) fn needs_maintenance(&self) -> bool {
        let Some(manifest) = self.manifest.as_ref() else {
            return true;
        };
        version_order(&manifest.bridge_version, WORKBENCH_BRIDGE_VERSION).is_lt()
            || (manifest.bridge_version == WORKBENCH_BRIDGE_VERSION
                && (manifest.protocol_version != WORKBENCH_BRIDGE_PROTOCOL_VERSION
                    || !manifest_matches_payload(manifest)
                    || bridge_payload().iter().any(|(name, content)| {
                        fs::read(self.directory.join(name))
                            .ok()
                            .is_none_or(|bytes| sha256(&bytes) != sha256(content.as_bytes()))
                    })))
    }

    pub(super) fn repair(&mut self) -> std::io::Result<bool> {
        if !self.needs_maintenance() {
            return Ok(false);
        }
        write_managed_files(self.directory)?;
        self.manifest = read_manifest(self.directory);
        Ok(true)
    }

    pub(super) fn migrate_legacy(&mut self, legacy: &Path) -> std::io::Result<bool> {
        if !migrate_legacy_bridge(legacy, self.directory)? {
            return Ok(false);
        }
        self.manifest = read_manifest(self.directory);
        Ok(true)
    }
}

fn read_manifest(directory: &Path) -> Option<BridgeManifest> {
    fs::read(directory.join("reforger-script-tools.manifest.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct BridgeManifest {
    pub(super) bridge_version: String,
    pub(super) protocol_version: u32,
    pub(super) files: Vec<BridgeManifestFile>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct BridgeManifestFile {
    pub(super) name: String,
    pub(super) sha256: String,
}

fn migrate_legacy_bridge(
    legacy_directory: &std::path::Path,
    bridge_directory: &std::path::Path,
) -> std::io::Result<bool> {
    let legacy_manifest_path = legacy_directory.join("reforger-script-tools.manifest.json");
    let Some(manifest) = fs::read(&legacy_manifest_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<BridgeManifest>(&bytes).ok())
    else {
        return Ok(false);
    };
    if !manifest_matches_payload(&manifest) {
        return Ok(false);
    }
    fs::create_dir_all(bridge_directory)?;
    for file in &manifest.files {
        let source = legacy_directory.join(&file.name);
        let destination = bridge_directory.join(&file.name);
        if destination.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "managed bridge migration destination already exists",
            ));
        }
        fs::rename(source, destination)?;
    }
    fs::rename(
        legacy_manifest_path,
        bridge_directory.join("reforger-script-tools.manifest.json"),
    )?;
    Ok(true)
}

pub(super) fn write_managed_files(bridge_directory: &std::path::Path) -> std::io::Result<()> {
    let payload = bridge_payload();
    if fs::symlink_metadata(bridge_directory)
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "managed bridge directory cannot be a symbolic link",
        ));
    }
    fs::create_dir_all(bridge_directory)?;
    let previous = read_manifest(bridge_directory);
    for (name, content) in payload {
        fs::write(bridge_directory.join(name), content)?;
    }
    let files = payload
        .iter()
        .map(|(name, content)| BridgeManifestFile {
            name: (*name).to_string(),
            sha256: sha256(content.as_bytes()),
        })
        .collect::<Vec<_>>();
    if let Some(previous) = previous {
        for file in previous.files {
            if is_managed_file_name(&file.name)
                && !files.iter().any(|current| current.name == file.name)
            {
                let _ = fs::remove_file(bridge_directory.join(file.name));
            }
        }
    }
    let manifest = BridgeManifest {
        bridge_version: WORKBENCH_BRIDGE_VERSION.to_string(),
        protocol_version: WORKBENCH_BRIDGE_PROTOCOL_VERSION,
        files,
    };
    fs::write(
        bridge_directory.join("reforger-script-tools.manifest.json"),
        serde_json::to_vec_pretty(&manifest).expect("bridge manifest serializes"),
    )
}

pub(super) fn manifest_matches_payload(manifest: &BridgeManifest) -> bool {
    let payload = bridge_payload();
    manifest.files.len() == payload.len()
        && payload.iter().all(|(name, content)| {
            let expected_hash = sha256(content.as_bytes());
            manifest
                .files
                .iter()
                .any(|file| file.name == *name && file.sha256 == expected_hash)
        })
}

fn is_managed_file_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && std::path::Path::new(name)
            .file_name()
            .is_some_and(|file| file == name)
}

pub(super) fn version_order(left: &str, right: &str) -> std::cmp::Ordering {
    match (Version::parse(left), Version::parse(right)) {
        (Ok(left), Ok(right)) => left.cmp(&right),
        _ if left == right => std::cmp::Ordering::Equal,
        // An unrecognized installed version is never safe to overwrite
        // automatically because its precedence cannot be proven.
        _ => std::cmp::Ordering::Greater,
    }
}

pub(super) fn bridge_payload() -> &'static [(&'static str, &'static str)] {
    &[
        ("RST_WorkbenchCapabilities.c", BRIDGE_CAPABILITIES_SOURCE),
        ("RST_WorkbenchState.c", BRIDGE_STATE_SOURCE),
        ("RST_WorkbenchListEditors.c", BRIDGE_LIST_EDITORS_SOURCE),
        ("RST_WorkbenchOpenEditor.c", BRIDGE_OPEN_EDITOR_SOURCE),
        ("RST_WorkbenchOpenResource.c", BRIDGE_OPEN_RESOURCE_SOURCE),
        ("RST_WorkbenchPlaySession.c", BRIDGE_PLAY_SESSION_SOURCE),
        (
            "RST_WorkbenchProjectContext.c",
            BRIDGE_PROJECT_CONTEXT_SOURCE,
        ),
        (
            "RST_WorkbenchLoadedAddonGraph.c",
            BRIDGE_LOADED_ADDON_GRAPH_SOURCE,
        ),
        (
            "RST_WorkbenchInspectResource.c",
            BRIDGE_INSPECT_RESOURCE_SOURCE,
        ),
        (
            "RST_WorkbenchWorldSelection.c",
            BRIDGE_WORLD_SELECTION_SOURCE,
        ),
        (
            "RST_WorkbenchSelectedEntityHierarchy.c",
            BRIDGE_SELECTED_ENTITY_HIERARCHY_SOURCE,
        ),
        ("RST_WorkbenchListEntities.c", BRIDGE_ENTITY_LIST_SOURCE),
        ("RST_WorkbenchSearchEntities.c", BRIDGE_ENTITY_SEARCH_SOURCE),
        ("RST_WorkbenchLayerState.c", BRIDGE_LAYER_STATE_SOURCE),
        ("RST_WorkbenchInspectEntity.c", BRIDGE_ENTITY_INSPECT_SOURCE),
        ("RST_WorkbenchSetSelection.c", BRIDGE_SET_SELECTION_SOURCE),
        (
            "RST_WorkbenchFindEntitiesByRadius.c",
            BRIDGE_ENTITY_RADIUS_QUERY_SOURCE,
        ),
        ("RST_WorkbenchSampleTerrain.c", BRIDGE_TERRAIN_SAMPLE_SOURCE),
        (
            "RST_WorkbenchViewportContext.c",
            BRIDGE_VIEWPORT_CONTEXT_SOURCE,
        ),
        ("RST_WorkbenchTrace.c", BRIDGE_TRACE_SOURCE),
        (
            "RST_WorkbenchClearSelection.c",
            BRIDGE_CLEAR_SELECTION_SOURCE,
        ),
        (
            "RST_WorkbenchEntityMutation.c",
            BRIDGE_ENTITY_MUTATION_SOURCE,
        ),
        ("RST_WorkbenchHistory.c", BRIDGE_HISTORY_SOURCE),
        ("RST_WorkbenchShapePoints.c", BRIDGE_SHAPE_POINTS_SOURCE),
        ("RST_WorkbenchShapeGeometry.c", BRIDGE_SHAPE_GEOMETRY_SOURCE),
        ("RST_WorkbenchSpline.c", BRIDGE_SPLINE_SOURCE),
        ("RST_WorkbenchComponents.c", BRIDGE_COMPONENTS_SOURCE),
        ("RST_WorkbenchProperties.c", BRIDGE_PROPERTIES_SOURCE),
        ("RST_WorkbenchPrefab.c", BRIDGE_PREFAB_SOURCE),
        ("RST_WorkbenchListResources.c", BRIDGE_LIST_RESOURCES_SOURCE),
    ]
}
