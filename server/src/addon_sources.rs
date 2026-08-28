use crate::addon_thumbnail_color::addon_thumbnail_color;
use crate::index::{SourceFileId, SymbolIndex};
use crate::index_build::{
    build_index_from_sources, IndexBuildControl, IndexBuildResult, IndexSourceText,
};
use crate::index_cache::{
    cache_format_identity, load_game_data_index_cache_with_control,
    load_or_build_archive_index_with_reuse_and_locator, read_index_cache_locator_section,
    write_atomic_bytes, GameDataIndexCacheResult, SourceFingerprint,
};
use crate::index_cache::{IndexCacheStatus, RuntimeIndexSummary};
use crate::model::{
    source_category_for_path, SourceFileMetadata, SourceKind, VirtualSourceIdentity,
    SOURCE_PRIORITY_GAME_DATA,
};
use crate::pack::{PakArchive, PakEntry, PakReader, PakSelection};
use crate::workbench::{installed_game_addon_project_files, registered_project_files};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};
use url::Url;

pub const BASE_GAME_GUID: &str = "58D0FB3206B6F859";
pub const ENFUSION_CORE_GUID: &str = "5614BBCCBB55ED1C";
pub const VIRTUAL_SOURCE_SCHEME: &str = "reforger-pak";
const MAX_ADDON_INDEX_WORKERS: usize = 4;
const ADDON_MANIFEST_HEADER_FILE: &str = "manifest-header.json";
const ADDON_MANIFEST_SCHEMA: &str = "reforger-addon-index-manifest-v4";
const ADDON_CACHE_CATALOGUE_FILE: &str = "cache-catalogue.json";
const LOCATOR_TABLE_MAGIC: &[u8; 8] = b"RSTLOC01";
const LOCATOR_TABLE_VERSION: u32 = 1;
const MAX_LOCATOR_RECORDS: usize = 1_000_000;
const MAX_LOCATOR_STRING_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddonScopeAuthority {
    WorkbenchLoaded,
    ProjectDependencies,
}

impl AddonScopeAuthority {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WorkbenchLoaded => "workbench-loaded",
            Self::ProjectDependencies => "project-dependencies-provisional",
        }
    }
}

pub struct LoadedAddonIndexResult {
    pub index: Arc<SymbolIndex>,
    pub source_line_starts: BTreeMap<SourceFileId, Vec<usize>>,
    pub scope_authority: AddonScopeAuthority,
    pub summary: RuntimeIndexSummary,
    pub rebuilt_instances: usize,
    pub loaded_instances: usize,
    /// Instances from the current Workbench graph which have no compatible
    /// published snapshot yet. Optimistic delivery omits only these instances
    /// until the validation pass builds and publishes them.
    pub missing_instances: usize,
    pub workspace_excluded_instances: usize,
    pub timings: LoadedAddonIndexTimings,
    pub instances: Vec<LoadedAddonIndexInstance>,
    pub unavailable_instances: Vec<UnavailableAddonIndexInstance>,
    pub scope_instances: Vec<LoadedAddonInstanceIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnavailableAddonIndexInstance {
    pub guid: String,
    pub display_id: String,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedAddonInstanceIdentity {
    pub guid: String,
    pub source_root: PathBuf,
}

/// Workbench-owned loaded add-on identity used by metadata-only catalogues.
/// This keeps source discovery in one route while allowing projections that do
/// not need the semantic symbol index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedAddonSourceInfo {
    pub guid: String,
    pub display_id: String,
    pub title: String,
    pub source_root: PathBuf,
}

/// Bounded performance facts for one add-on scope refresh.
/// Paths and source text deliberately stay out of this reporting surface.
#[derive(Debug, Clone, Copy, Default)]
pub struct LoadedAddonIndexTimings {
    pub graph_read: Duration,
    pub workspace_root_resolution: Duration,
    pub cache_prune: Duration,
    /// Sum of per-instance time spent reading cache metadata outside
    /// `symbols.bin`. Validation workers run in parallel, so this is work
    /// rather than the critical-path duration; `index_load_or_build` remains
    /// the wall-clock measure for that stage.
    pub cache_metadata_read: Duration,
    /// Wall-clock time spent proving the live loaded source contents before a
    /// cache can be trusted. This includes archive selection and its strong
    /// content identity, not source parsing.
    pub source_inspection: Duration,
    /// Wall-clock time spent loading or rebuilding the inspected add-on
    /// indexes. This is intentionally separate from inspection so warm-cache
    /// regressions cannot be mistaken for source-verification cost.
    pub index_load_or_build: Duration,
    pub layer_rebase: Duration,
    pub layer_file_projection: Duration,
    pub layer_lookup_projection: Duration,
    pub layer_compose: Duration,
    pub total: Duration,
}

/// One non-workspace add-on's cache and indexing outcome from a scope refresh.
#[derive(Debug, Clone)]
pub struct LoadedAddonIndexInstance {
    pub guid: String,
    pub display_id: String,
    pub title: String,
    pub thumbnail_color: Option<String>,
    pub pack_count: usize,
    pub script_count: usize,
    pub file_start: usize,
    pub file_count: usize,
    pub cache_path: PathBuf,
    pub revision: String,
    pub cache_status: String,
    pub cache_detail: Option<String>,
    pub summary: RuntimeIndexSummary,
    pub timings: crate::index_cache::IndexCacheTimings,
    pub cache_file_bytes: Option<u64>,
}

struct InspectedAddonTask {
    sequence: usize,
    addon: LoadedAddonSource,
    inspection: BaseGameInspection,
    inspection_elapsed: Duration,
}

struct PendingAddonInspection {
    sequence: usize,
    addon: LoadedAddonSource,
}

struct CompletedAddonInspection {
    sequence: usize,
    result: Result<InspectedAddonTask, String>,
}

struct CompletedAddonTask {
    sequence: usize,
    addon: LoadedAddonSource,
    thumbnail_color: Option<String>,
    result: Result<GameDataIndexCacheResult, String>,
}

struct CachedAddonDescriptor {
    sequence: usize,
    addon: LoadedAddonSource,
    cache_path: PathBuf,
}

struct CompletedCachedAddonLoad {
    sequence: usize,
    addon: LoadedAddonSource,
    cache_path: PathBuf,
    result: Result<Option<GameDataIndexCacheResult>, String>,
}

struct CompletedCachedManifestLoad {
    sequence: usize,
    manifest: AddonIndexManifestHeader,
    cache_path: PathBuf,
    result: Result<Option<GameDataIndexCacheResult>, String>,
}

struct CachedManifestDescriptor {
    sequence: usize,
    manifest: AddonIndexManifestHeader,
    cache_path: PathBuf,
}

#[derive(Debug, Deserialize)]
struct Inventory {
    #[allow(dead_code)]
    schema: String,
    roots: Vec<InventoryRoot>,
    #[serde(default)]
    addons: Vec<InventoryAddon>,
}

#[derive(Debug, Deserialize)]
struct InventoryRoot {
    kind: String,
    path: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InventoryAddon {
    root_kind: String,
    directory_name: String,
    path: PathBuf,
    project_file: Option<PathBuf>,
    #[serde(default)]
    pack_files: Vec<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkbenchLoadedAddonGraphInventory {
    schema: String,
    bridge_version: String,
    protocol_version: u32,
    addons: Vec<WorkbenchLoadedAddonInventory>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkbenchLoadedAddonInventory {
    guid: String,
    id: String,
    title: String,
    source_root: PathBuf,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct LoadedAddonSource {
    guid: String,
    id: String,
    title: String,
    source_root: PathBuf,
}

#[derive(Debug)]
struct LoadedAddonGraph {
    addons: Vec<LoadedAddonSource>,
}

struct BaseGameInspection {
    guid: String,
    display_id: String,
    root: PathBuf,
    thumbnail_color: Option<String>,
    archives: Vec<(PakArchive, Vec<PakEntry>)>,
    loose_files: Vec<PathBuf>,
    fingerprint: SourceFingerprint,
    artifact_digest: String,
    artifacts: Vec<PackArtifact>,
    scripts: Vec<ScriptLocator>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PackArtifact {
    relative_path: String,
    bytes: u64,
    modified_unix_ms: u128,
    selected_payload_sha256: String,
    strong_manifest_sha512: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScriptLocator {
    uri: String,
    logical_path: String,
    pack_relative_path: String,
    offset: u64,
    compressed_length: u64,
    original_length: u64,
    compression: u32,
    compressed_payload_sha256: String,
}

fn encode_locator_table(scripts: &[ScriptLocator]) -> Result<Vec<u8>, String> {
    let mut ordered = scripts.iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| {
        left.logical_path
            .cmp(&right.logical_path)
            .then_with(|| left.pack_relative_path.cmp(&right.pack_relative_path))
            .then_with(|| left.offset.cmp(&right.offset))
    });
    let mut pack_paths = BTreeMap::<String, u32>::new();
    for script in &ordered {
        let next = u32::try_from(pack_paths.len())
            .map_err(|_| "Too many packed source paths in locator table".to_string())?;
        pack_paths
            .entry(script.pack_relative_path.clone())
            .or_insert(next);
    }
    let mut bytes = Vec::new();
    bytes.extend_from_slice(LOCATOR_TABLE_MAGIC);
    bytes.extend_from_slice(&LOCATOR_TABLE_VERSION.to_le_bytes());
    bytes.extend_from_slice(
        &u32::try_from(ordered.len())
            .map_err(|_| "Too many source locators".to_string())?
            .to_le_bytes(),
    );
    bytes.extend_from_slice(
        &u32::try_from(pack_paths.len())
            .map_err(|_| "Too many packed source paths".to_string())?
            .to_le_bytes(),
    );
    for path in pack_paths.keys() {
        write_locator_string(&mut bytes, path)?;
    }
    for script in ordered {
        write_locator_string(&mut bytes, &script.logical_path)?;
        let pack_path_index = *pack_paths
            .get(&script.pack_relative_path)
            .ok_or_else(|| "Locator table pack path was not interned".to_string())?;
        bytes.extend_from_slice(&pack_path_index.to_le_bytes());
        bytes.extend_from_slice(&script.offset.to_le_bytes());
        bytes.extend_from_slice(&script.compressed_length.to_le_bytes());
        bytes.extend_from_slice(&script.original_length.to_le_bytes());
        bytes.extend_from_slice(&script.compression.to_le_bytes());
        bytes.extend_from_slice(&locator_digest_bytes(&script.compressed_payload_sha256)?);
    }
    Ok(bytes)
}

fn write_locator_string(bytes: &mut Vec<u8>, value: &str) -> Result<(), String> {
    let value = value.as_bytes();
    let length =
        u32::try_from(value.len()).map_err(|_| "Locator table string is too large".to_string())?;
    bytes.extend_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(value);
    Ok(())
}

fn locator_digest_bytes(value: &str) -> Result<[u8; 32], String> {
    if value.is_empty() {
        return Ok([0; 32]);
    }
    if value.len() != 64 {
        return Err("Locator payload digest is not a 256-bit hexadecimal value".to_string());
    }
    let mut digest = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let high = hex_digit(pair[0])?;
        let low = hex_digit(pair[1])?;
        digest[index] = (high << 4) | low;
    }
    Ok(digest)
}

fn hex_digit(value: u8) -> Result<u8, String> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err("Locator payload digest contains a non-hexadecimal digit".to_string()),
    }
}

fn locator_digest_string(value: &[u8]) -> String {
    if value.iter().all(|byte| *byte == 0) {
        return String::new();
    }
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode_locator_table(bytes: &[u8]) -> Result<Vec<ScriptLocator>, String> {
    let mut cursor = bytes;
    let magic = take_locator_bytes(&mut cursor, LOCATOR_TABLE_MAGIC.len())?;
    if magic != LOCATOR_TABLE_MAGIC {
        return Err("Index cache locator section magic mismatch".to_string());
    }
    let version = take_locator_u32(&mut cursor)?;
    if version != LOCATOR_TABLE_VERSION {
        return Err(format!("Unsupported index cache locator version {version}"));
    }
    let record_count = usize::try_from(take_locator_u32(&mut cursor)?)
        .map_err(|_| "Locator record count is too large".to_string())?;
    if record_count > MAX_LOCATOR_RECORDS {
        return Err("Locator record count exceeds the cache limit".to_string());
    }
    let pack_path_count = usize::try_from(take_locator_u32(&mut cursor)?)
        .map_err(|_| "Locator pack path count is too large".to_string())?;
    if pack_path_count > MAX_LOCATOR_RECORDS {
        return Err("Locator pack path count exceeds the cache limit".to_string());
    }
    let mut pack_paths = Vec::with_capacity(pack_path_count);
    for _ in 0..pack_path_count {
        pack_paths.push(take_locator_string(&mut cursor)?);
    }
    let mut scripts = Vec::with_capacity(record_count);
    for _ in 0..record_count {
        let logical_path = take_locator_string(&mut cursor)?;
        let pack_path_index = usize::try_from(take_locator_u32(&mut cursor)?)
            .map_err(|_| "Locator pack path index is too large".to_string())?;
        let pack_relative_path = pack_paths
            .get(pack_path_index)
            .ok_or_else(|| "Locator pack path index is out of bounds".to_string())?
            .clone();
        let offset = take_locator_u64(&mut cursor)?;
        let compressed_length = take_locator_u64(&mut cursor)?;
        let original_length = take_locator_u64(&mut cursor)?;
        let compression = take_locator_u32(&mut cursor)?;
        let digest = locator_digest_string(take_locator_bytes(&mut cursor, 32)?);
        scripts.push(ScriptLocator {
            uri: String::new(),
            logical_path,
            pack_relative_path,
            offset,
            compressed_length,
            original_length,
            compression,
            compressed_payload_sha256: digest,
        });
    }
    if !cursor.is_empty() {
        return Err("Index cache locator section contains trailing bytes".to_string());
    }
    Ok(scripts)
}

fn take_locator_bytes<'a>(cursor: &mut &'a [u8], length: usize) -> Result<&'a [u8], String> {
    if cursor.len() < length {
        return Err("Index cache locator section is truncated".to_string());
    }
    let (value, rest) = cursor.split_at(length);
    *cursor = rest;
    Ok(value)
}

fn take_locator_u32(cursor: &mut &[u8]) -> Result<u32, String> {
    Ok(u32::from_le_bytes(
        take_locator_bytes(cursor, 4)?.try_into().unwrap(),
    ))
}

fn take_locator_u64(cursor: &mut &[u8]) -> Result<u64, String> {
    Ok(u64::from_le_bytes(
        take_locator_bytes(cursor, 8)?.try_into().unwrap(),
    ))
}

fn take_locator_string(cursor: &mut &[u8]) -> Result<String, String> {
    let length = usize::try_from(take_locator_u32(cursor)?)
        .map_err(|_| "Locator string length is too large".to_string())?;
    if length > MAX_LOCATOR_STRING_BYTES {
        return Err("Locator string exceeds the cache limit".to_string());
    }
    let value = take_locator_bytes(cursor, length)?;
    String::from_utf8(value.to_vec())
        .map_err(|_| "Index cache locator section contains invalid UTF-8".to_string())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddonIndexManifest {
    schema: String,
    cache_schema: String,
    cache_format_version: u32,
    cache_index_shape: String,
    extractor_schema: String,
    guid: String,
    display_id: String,
    #[serde(default)]
    thumbnail_color: Option<String>,
    source_root: PathBuf,
    source_precedence: String,
    revision: String,
    pack_count: usize,
    script_count: usize,
    pack_artifacts: Vec<PackArtifact>,
    scripts: Vec<ScriptLocator>,
    index_file: String,
    index_bytes: u64,
}

/// The fields needed to validate a cache are deliberately separate from the
/// locator-rich manifest. The latter remains available for source URI
/// inspection, while this compact header keeps the warm validation path from
/// deserializing every script locator on every startup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddonIndexManifestHeader {
    schema: String,
    cache_schema: String,
    cache_format_version: u32,
    cache_index_shape: String,
    extractor_schema: String,
    guid: String,
    display_id: String,
    #[serde(default)]
    thumbnail_color: Option<String>,
    source_root: PathBuf,
    source_precedence: String,
    revision: String,
    pack_count: usize,
    script_count: usize,
    pack_artifacts: Vec<PackArtifact>,
    index_file: String,
    index_bytes: u64,
    #[serde(default)]
    manifest_sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddonCacheCatalogue {
    schema: String,
    entries: Vec<AddonIndexManifestHeader>,
}

impl AddonIndexManifest {
    fn header(&self) -> AddonIndexManifestHeader {
        AddonIndexManifestHeader {
            schema: self.schema.clone(),
            cache_schema: self.cache_schema.clone(),
            cache_format_version: self.cache_format_version,
            cache_index_shape: self.cache_index_shape.clone(),
            extractor_schema: self.extractor_schema.clone(),
            guid: self.guid.clone(),
            display_id: self.display_id.clone(),
            thumbnail_color: self.thumbnail_color.clone(),
            source_root: self.source_root.clone(),
            source_precedence: self.source_precedence.clone(),
            revision: self.revision.clone(),
            pack_count: self.pack_count,
            script_count: self.script_count,
            pack_artifacts: self.pack_artifacts.clone(),
            index_file: self.index_file.clone(),
            index_bytes: self.index_bytes,
            manifest_sha256: None,
        }
    }
}

fn cached_thumbnail_color(cache_path: &Path) -> Option<String> {
    let header_path = cache_path.parent()?.join(ADDON_MANIFEST_HEADER_FILE);
    let header =
        serde_json::from_slice::<AddonIndexManifestHeader>(&fs::read(header_path).ok()?).ok()?;
    header.thumbnail_color
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct InventoryAddonManifest {
    schema: &'static str,
    guid: String,
    display_id: String,
    directory_name: String,
    root_kind: String,
    source_root: PathBuf,
    semantic_status: &'static str,
    pack_artifacts: Vec<InventoryPackArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct InventoryPackArtifact {
    path: PathBuf,
    bytes: u64,
    modified_unix_ms: u128,
    strong_manifest_sha512: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InventoryPublication {
    schema: String,
    revision: String,
}

#[derive(Debug)]
struct PackedSourceRevision {
    artifacts: Vec<ArtifactStamp>,
    entries: Vec<PackedSourceEntry>,
}

#[derive(Debug)]
struct PackedSourceEntry {
    entry: PakEntry,
    compressed_payload_sha256: String,
}

#[derive(Debug)]
struct ArtifactStamp {
    path: PathBuf,
    bytes: u64,
    modified_unix_ms: u128,
}

static SOURCE_REVISIONS: OnceLock<Mutex<BTreeMap<String, Arc<PackedSourceRevision>>>> =
    OnceLock::new();
static SOURCE_REVISION_ROOTS: OnceLock<Mutex<BTreeMap<String, PathBuf>>> = OnceLock::new();

/// Builds the installed base-game index directly from selected PAC entries.
/// User add-ons remain inventory-only until load ordering is implemented.
pub fn build_base_game_index(
    inventory_path: &Path,
    control: &IndexBuildControl,
) -> Result<IndexBuildResult, String> {
    let inspection = inspect_base_game(inventory_path, control)?;
    let sources = packed_source_revision(&inspection);
    build_inspected_base_game(
        inspection,
        &sources,
        control,
        standalone_source_build_worker_count(),
    )
}

pub fn load_or_build_base_game_index(
    inventory_path: &Path,
    storage_root: &Path,
    control: &IndexBuildControl,
) -> Result<GameDataIndexCacheResult, String> {
    let inspection_started = std::time::Instant::now();
    let inspection = inspect_base_game(inventory_path, control)?;
    let result = load_or_build_inspected_addon(
        inspection,
        storage_root,
        control,
        inspection_started.elapsed(),
        standalone_source_build_worker_count(),
    )?;
    let _ = refresh_cache_catalogue(storage_root);
    Ok(result)
}

/// Builds an independent compact index for every packed add-on that the live
/// Workbench graph names, then composes those immutable indexes by stable
/// instance identity without copying their symbol records. A graph entry is
/// never inferred from a directory scan.
pub fn load_or_build_loaded_addon_indexes(
    inventory_path: &Path,
    storage_root: &Path,
    workspace_roots: &[PathBuf],
    control: &IndexBuildControl,
) -> Result<LoadedAddonIndexResult, String> {
    let graph_start = Instant::now();
    let graph = read_loaded_addon_graph(inventory_path)?;
    let graph_read = graph_start.elapsed();
    load_or_build_addon_indexes(
        graph,
        graph_read,
        storage_root,
        workspace_roots,
        control,
        AddonScopeAuthority::WorkbenchLoaded,
    )
}

pub fn load_or_build_base_game_indexes(
    inventory_path: &Path,
    storage_root: &Path,
    workspace_roots: &[PathBuf],
    control: &IndexBuildControl,
) -> Result<LoadedAddonIndexResult, String> {
    let graph_start = Instant::now();
    let mut graph = read_loaded_addon_graph(inventory_path)?;
    let graph_read = graph_start.elapsed();
    graph.addons.retain(is_base_game_addon);
    load_or_build_addon_indexes(
        graph,
        graph_read,
        storage_root,
        workspace_roots,
        control,
        AddonScopeAuthority::WorkbenchLoaded,
    )
}

fn load_or_build_addon_indexes(
    graph: LoadedAddonGraph,
    graph_read: Duration,
    storage_root: &Path,
    workspace_roots: &[PathBuf],
    control: &IndexBuildControl,
    scope_authority: AddonScopeAuthority,
) -> Result<LoadedAddonIndexResult, String> {
    let total_start = Instant::now();
    let workspace_root_start = Instant::now();
    let workspace_roots = workspace_roots
        .iter()
        .filter_map(|root| fs::canonicalize(root).ok())
        .collect::<Vec<_>>();
    let workspace_root_resolution = workspace_root_start.elapsed();
    let addons = graph.addons;
    let cache_prune_start = Instant::now();
    if scope_authority == AddonScopeAuthority::WorkbenchLoaded {
        prune_unloaded_addon_caches(storage_root, &addons)?;
    }
    let cache_prune = cache_prune_start.elapsed();
    let mut pending_inspections = Vec::with_capacity(addons.len());
    let mut summary = RuntimeIndexSummary::default();
    let mut rebuilt_instances = 0;
    let mut loaded_instances = 0;
    let mut workspace_excluded_instances = 0;
    let source_inspection_start = Instant::now();
    for (sequence, addon) in addons.into_iter().enumerate() {
        control.check()?;
        if workspace_roots
            .iter()
            .any(|workspace_root| workspace_root.starts_with(&addon.source_root))
        {
            remove_workspace_addon_cache(storage_root, &addon)?;
            workspace_excluded_instances += 1;
            continue;
        }
        pending_inspections.push(PendingAddonInspection { sequence, addon });
    }
    let inspection_task_count = pending_inspections.len();
    let pending_inspections = Arc::new(Mutex::new(pending_inspections));
    let (inspection_sender, inspection_receiver) = mpsc::channel();
    let mut inspection_workers =
        Vec::with_capacity(addon_inspection_worker_count(inspection_task_count));
    for _ in 0..addon_inspection_worker_count(inspection_task_count) {
        let pending_inspections = pending_inspections.clone();
        let inspection_sender = inspection_sender.clone();
        let control = control.clone();
        inspection_workers.push(thread::spawn(move || loop {
            let pending = pending_inspections.lock().unwrap().pop();
            let Some(pending) = pending else {
                return;
            };
            let sequence = pending.sequence;
            let result = control.check().and_then(|()| {
                let started = Instant::now();
                let archives = addon_archive_paths(&pending.addon.source_root)?;
                let inspection = inspect_packed_addon(
                    pending.addon.guid.clone(),
                    format!("{} ({})", pending.addon.id, pending.addon.title),
                    pending.addon.source_root.clone(),
                    archives,
                    &control,
                )?;
                Ok(InspectedAddonTask {
                    sequence,
                    addon: pending.addon,
                    inspection,
                    inspection_elapsed: started.elapsed(),
                })
            });
            let _ = inspection_sender.send(CompletedAddonInspection { sequence, result });
        }));
    }
    drop(inspection_sender);
    let mut tasks = Vec::with_capacity(inspection_task_count);
    for _ in 0..inspection_task_count {
        let completed = inspection_receiver
            .recv()
            .map_err(|error| format!("Add-on inspection worker ended unexpectedly: {error}"))?;
        tasks.push(completed);
    }
    for worker in inspection_workers {
        worker
            .join()
            .map_err(|_| "Add-on inspection worker panicked".to_string())?;
    }
    tasks.sort_by_key(|task| task.sequence);
    let mut tasks = tasks
        .into_iter()
        .map(|task| task.result)
        .collect::<Result<Vec<_>, _>>()?;
    let source_inspection = source_inspection_start.elapsed();
    let task_count = tasks.len();
    // Largest script sets start first, so the bounded worker pool overlaps the
    // known long tail without attaching behavior to a particular add-on name.
    tasks.sort_by(|left, right| {
        let left_weight = left.inspection.scripts.len() + left.inspection.loose_files.len();
        let right_weight = right.inspection.scripts.len() + right.inspection.loose_files.len();
        left_weight
            .cmp(&right_weight)
            .then_with(|| right.sequence.cmp(&left.sequence))
    });
    let tasks = Arc::new(Mutex::new(tasks));
    let (completed_sender, completed_receiver) = mpsc::channel();
    let index_load_or_build_start = Instant::now();
    let worker_count = addon_index_worker_count(storage_root, task_count)?;
    let source_build_worker_count = source_build_worker_count(worker_count);
    let mut workers = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let tasks = tasks.clone();
        let completed_sender = completed_sender.clone();
        let storage_root = storage_root.to_path_buf();
        let control = control.clone();
        workers.push(thread::spawn(move || loop {
            let task = tasks.lock().unwrap().pop();
            let Some(task) = task else {
                return;
            };
            let thumbnail_color = task.inspection.thumbnail_color.clone();
            let result = control.check().and_then(|()| {
                load_or_build_inspected_addon(
                    task.inspection,
                    &storage_root,
                    &control,
                    task.inspection_elapsed,
                    source_build_worker_count,
                )
            });
            let _ = completed_sender.send(CompletedAddonTask {
                sequence: task.sequence,
                addon: task.addon,
                thumbnail_color,
                result,
            });
        }));
    }
    drop(completed_sender);
    let mut completed = Vec::with_capacity(task_count);
    for _ in 0..task_count {
        completed.push(
            completed_receiver
                .recv()
                .map_err(|error| format!("Add-on index worker ended unexpectedly: {error}"))?,
        );
    }
    for worker in workers {
        worker
            .join()
            .map_err(|_| "Add-on index worker panicked".to_string())?;
    }
    let index_load_or_build = index_load_or_build_start.elapsed();
    let cache_metadata_read = completed
        .iter()
        .filter_map(|task| task.result.as_ref().ok())
        .map(|result| result.timings.cache_metadata_read)
        .sum();
    completed.sort_by_key(|task| task.sequence);
    let mut indexes = Vec::with_capacity(completed.len());
    let mut source_line_starts = BTreeMap::new();
    let mut instances = Vec::with_capacity(completed.len());
    let mut scope_instances = Vec::with_capacity(completed.len());
    let mut file_start = 0;
    for completed in completed {
        let addon = completed.addon;
        let thumbnail_color = completed.thumbnail_color;
        let result = completed.result?;
        match &result.cache_status {
            IndexCacheStatus::Loaded => loaded_instances += 1,
            IndexCacheStatus::Rebuilt { .. } => rebuilt_instances += 1,
        }
        summary.files += result.summary.files;
        summary.bytes += result.summary.bytes;
        summary.indexed_symbols += result.summary.indexed_symbols;
        summary.parse_diagnostics += result.summary.parse_diagnostics;
        summary.lossy_files += result.summary.lossy_files;
        let (pack_count, script_count, revision) = match &result.fingerprint {
            SourceFingerprint::Addon {
                pack_count,
                catalogue_entry_count,
                artifact_digest,
                ..
            } => (*pack_count, *catalogue_entry_count, artifact_digest.clone()),
            _ => unreachable!("loaded add-on indexing always has an add-on fingerprint"),
        };
        let file_count = result.index.files().len();
        source_line_starts.extend(
            result
                .source_line_starts
                .iter()
                .map(|(file, starts)| (SourceFileId(file.0 + file_start), starts.clone())),
        );
        let cache_path = storage_root
            .join(addon_instance_key(&addon.guid, &addon.source_root))
            .join("symbols.bin");
        instances.push(LoadedAddonIndexInstance {
            guid: addon.guid.clone(),
            display_id: addon.id.clone(),
            title: addon.title.clone(),
            thumbnail_color,
            pack_count,
            script_count,
            file_start,
            file_count,
            cache_path,
            revision,
            cache_status: result.cache_status.as_str().to_string(),
            cache_detail: result.cache_status.detail().map(str::to_string),
            summary: result.summary.clone(),
            timings: result.timings,
            cache_file_bytes: result.cache_file_bytes,
        });
        scope_instances.push(addon_scope_identity(&addon));
        indexes.push(result.index);
        file_start += file_count;
    }
    let (index, layer_timings) = SymbolIndex::layered_with_timings(indexes);
    let result = LoadedAddonIndexResult {
        index: Arc::new(index),
        source_line_starts,
        scope_authority,
        summary,
        rebuilt_instances,
        loaded_instances,
        missing_instances: 0,
        workspace_excluded_instances,
        timings: LoadedAddonIndexTimings {
            graph_read,
            workspace_root_resolution,
            cache_prune,
            cache_metadata_read,
            source_inspection,
            index_load_or_build,
            layer_rebase: layer_timings.rebase,
            layer_file_projection: layer_timings.file_projection,
            layer_lookup_projection: layer_timings.lookup_projection,
            layer_compose: layer_timings.total,
            total: total_start.elapsed(),
        },
        instances,
        unavailable_instances: Vec::new(),
        scope_instances,
    };
    let _ = refresh_cache_catalogue(storage_root);
    Ok(result)
}

/// Delivers every compatible cache named by the current Workbench graph
/// without inspecting manifests or packed/loose source bytes. The exact
/// `(GUID, source-root)` instance key locates `symbols.bin`; its self-describing
/// header proves cache compatibility and add-on identity. The validation pass
/// always follows this function and is the sole authority that replaces a
/// stale snapshot. Both source forms share this exact lifecycle.
pub fn load_cached_loaded_addon_indexes(
    inventory_path: &Path,
    storage_root: &Path,
    workspace_roots: &[PathBuf],
    control: &IndexBuildControl,
) -> Result<LoadedAddonIndexResult, String> {
    load_cached_loaded_addon_indexes_with_maintenance(
        inventory_path,
        storage_root,
        workspace_roots,
        control,
        true,
    )
}

/// Reads the exact loaded graph's compatible indexes without pruning,
/// rebuilding, or otherwise mutating the shared parser-owned cache storage.
pub fn read_cached_loaded_addon_indexes(
    inventory_path: &Path,
    storage_root: &Path,
    workspace_roots: &[PathBuf],
    control: &IndexBuildControl,
) -> Result<LoadedAddonIndexResult, String> {
    load_cached_loaded_addon_indexes_with_maintenance(
        inventory_path,
        storage_root,
        workspace_roots,
        control,
        false,
    )
}

fn load_cached_loaded_addon_indexes_with_maintenance(
    inventory_path: &Path,
    storage_root: &Path,
    workspace_roots: &[PathBuf],
    control: &IndexBuildControl,
    maintain_storage: bool,
) -> Result<LoadedAddonIndexResult, String> {
    let total_start = Instant::now();
    let graph_start = Instant::now();
    let graph = read_loaded_addon_graph(inventory_path)?;
    let graph_read = graph_start.elapsed();
    let workspace_root_start = Instant::now();
    let workspace_roots = workspace_roots
        .iter()
        .filter_map(|root| fs::canonicalize(root).ok())
        .collect::<Vec<_>>();
    let workspace_root_resolution = workspace_root_start.elapsed();
    let addons = graph.addons;
    let cache_prune_start = Instant::now();
    if maintain_storage {
        prune_unloaded_addon_caches(storage_root, &addons)?;
    }
    let cache_prune = cache_prune_start.elapsed();
    let mut pending = Vec::with_capacity(addons.len());
    let mut workspace_excluded_instances = 0;
    for (sequence, addon) in addons.into_iter().enumerate() {
        control.check()?;
        if workspace_roots
            .iter()
            .any(|workspace_root| workspace_root.starts_with(&addon.source_root))
        {
            if maintain_storage {
                remove_workspace_addon_cache(storage_root, &addon)?;
            }
            workspace_excluded_instances += 1;
            continue;
        }
        pending.push((sequence, addon));
    }

    let descriptors = pending
        .into_iter()
        .map(|(sequence, addon)| CachedAddonDescriptor {
            sequence,
            cache_path: storage_root
                .join(addon_instance_key(&addon.guid, &addon.source_root))
                .join("symbols.bin"),
            addon,
        })
        .collect::<Vec<_>>();

    let cache_load_start = Instant::now();
    let descriptors = Arc::new(Mutex::new(descriptors));
    let (cache_sender, cache_receiver) = mpsc::channel();
    let cache_task_count = descriptors.lock().unwrap().len();
    let cache_worker_count = addon_index_worker_count(storage_root, cache_task_count)?;
    let mut cache_workers = Vec::with_capacity(cache_worker_count);
    for _ in 0..cache_worker_count {
        let descriptors = descriptors.clone();
        let cache_sender = cache_sender.clone();
        let control = control.clone();
        cache_workers.push(thread::spawn(move || loop {
            let Some(descriptor) = descriptors.lock().unwrap().pop() else {
                return;
            };
            let result = control.check().and_then(|()| {
                let cached =
                    match load_game_data_index_cache_with_control(&descriptor.cache_path, &control)
                    {
                        Ok(cached) => cached,
                        Err(error) if control.is_cancelled() => return Err(error),
                        Err(_) => None,
                    };
                let Some(cached) = cached.filter(|cached| {
                    matches!(
                        &cached.fingerprint,
                        SourceFingerprint::Addon { guid, .. }
                            if guid.eq_ignore_ascii_case(&descriptor.addon.guid)
                    )
                }) else {
                    return Ok(None);
                };
                if let (
                    Some(cache_root),
                    SourceFingerprint::Addon {
                        guid,
                        artifact_digest,
                        ..
                    },
                ) = (descriptor.cache_path.parent(), &cached.fingerprint)
                {
                    register_cached_source_revision_root(guid, artifact_digest, cache_root);
                }
                Ok(Some(cached))
            });
            let _ = cache_sender.send(CompletedCachedAddonLoad {
                sequence: descriptor.sequence,
                addon: descriptor.addon,
                cache_path: descriptor.cache_path,
                result,
            });
        }));
    }
    drop(cache_sender);
    let mut completed = Vec::with_capacity(cache_task_count);
    for _ in 0..cache_task_count {
        completed.push(
            cache_receiver
                .recv()
                .map_err(|error| format!("Add-on cache load worker ended unexpectedly: {error}"))?,
        );
    }
    for worker in cache_workers {
        worker
            .join()
            .map_err(|_| "Add-on cache load worker panicked".to_string())?;
    }
    let index_load_or_build = cache_load_start.elapsed();
    completed.sort_by_key(|completed| completed.sequence);
    let mut summary = RuntimeIndexSummary::default();
    let mut loaded_instances = 0;
    let mut missing_instances = 0;
    let mut indexes = Vec::with_capacity(completed.len());
    let mut source_line_starts = BTreeMap::new();
    let mut instances = Vec::with_capacity(completed.len());
    let mut unavailable_instances = Vec::new();
    let mut scope_instances = Vec::with_capacity(completed.len());
    let mut file_start = 0;
    let mut cache_metadata_read = Duration::ZERO;
    for completed in completed {
        let addon = completed.addon;
        let cache_path = completed.cache_path;
        let Some(result) = completed.result? else {
            missing_instances += 1;
            unavailable_instances.push(UnavailableAddonIndexInstance {
                guid: addon.guid,
                display_id: addon.id,
                title: addon.title,
            });
            continue;
        };
        loaded_instances += 1;
        summary.files += result.summary.files;
        summary.bytes += result.summary.bytes;
        summary.indexed_symbols += result.summary.indexed_symbols;
        summary.parse_diagnostics += result.summary.parse_diagnostics;
        summary.lossy_files += result.summary.lossy_files;
        let (pack_count, script_count, revision) = match &result.fingerprint {
            SourceFingerprint::Addon {
                pack_count,
                catalogue_entry_count,
                artifact_digest,
                ..
            } => (*pack_count, *catalogue_entry_count, artifact_digest.clone()),
            _ => unreachable!("loaded add-on cache always has an add-on fingerprint"),
        };
        let file_count = result.index.files().len();
        source_line_starts.extend(
            result
                .source_line_starts
                .iter()
                .map(|(file, starts)| (SourceFileId(file.0 + file_start), starts.clone())),
        );
        let thumbnail_read_start = Instant::now();
        let thumbnail_color = cached_thumbnail_color(&cache_path);
        cache_metadata_read += thumbnail_read_start.elapsed();
        instances.push(LoadedAddonIndexInstance {
            guid: addon.guid.clone(),
            display_id: addon.id.clone(),
            title: addon.title.clone(),
            thumbnail_color,
            pack_count,
            script_count,
            file_start,
            file_count,
            cache_path,
            revision,
            cache_status: "optimistic-loaded".to_string(),
            cache_detail: Some("source-validation-pending".to_string()),
            summary: result.summary.clone(),
            timings: result.timings,
            cache_file_bytes: result.cache_file_bytes,
        });
        scope_instances.push(addon_scope_identity(&addon));
        indexes.push(result.index);
        file_start += file_count;
    }
    let (index, layer_timings) = SymbolIndex::layered_with_timings(indexes);
    Ok(LoadedAddonIndexResult {
        index: Arc::new(index),
        source_line_starts,
        scope_authority: AddonScopeAuthority::WorkbenchLoaded,
        summary,
        rebuilt_instances: 0,
        loaded_instances,
        missing_instances,
        workspace_excluded_instances,
        timings: LoadedAddonIndexTimings {
            graph_read,
            workspace_root_resolution,
            cache_prune,
            cache_metadata_read,
            source_inspection: Duration::ZERO,
            index_load_or_build,
            layer_rebase: layer_timings.rebase,
            layer_file_projection: layer_timings.file_projection,
            layer_lookup_projection: layer_timings.lookup_projection,
            layer_compose: layer_timings.total,
            total: total_start.elapsed(),
        },
        instances,
        unavailable_instances,
        scope_instances,
    })
}

/// Validates the current source identity for an already-published exact graph
/// without decoding, rebuilding, or recomposing any cached index. `false`
/// means that the caller must run the authoritative replacement path.
pub fn loaded_addon_sources_are_current(
    inventory_path: &Path,
    storage_root: &Path,
    workspace_roots: &[PathBuf],
    control: &IndexBuildControl,
) -> Result<bool, String> {
    let graph = read_loaded_addon_graph(inventory_path)?;
    let workspace_roots = workspace_roots
        .iter()
        .filter_map(|root| fs::canonicalize(root).ok())
        .collect::<Vec<_>>();
    for addon in graph.addons {
        control.check()?;
        if workspace_roots
            .iter()
            .any(|workspace_root| workspace_root.starts_with(&addon.source_root))
        {
            continue;
        }
        let archives = addon_archive_paths(&addon.source_root)?;
        let inspection = inspect_packed_addon(
            addon.guid,
            format!("{} ({})", addon.id, addon.title),
            addon.source_root,
            archives,
            control,
        )?;
        if !cached_manifest_matches_inspection(&inspection, storage_root)? {
            return Ok(false);
        }
    }
    Ok(true)
}

pub fn load_all_cached_addon_indexes(
    storage_root: &Path,
    workspace_roots: &[PathBuf],
    control: &IndexBuildControl,
) -> Result<LoadedAddonIndexResult, String> {
    load_cached_indexes_from_storage(storage_root, workspace_roots, control, false, None)
}

pub fn load_cached_base_game_indexes(
    storage_root: &Path,
    workspace_roots: &[PathBuf],
    control: &IndexBuildControl,
) -> Result<LoadedAddonIndexResult, String> {
    load_cached_indexes_from_storage(storage_root, workspace_roots, control, true, None)
}

pub fn load_cached_dependency_addon_indexes(
    project_files: &[PathBuf],
    storage_root: &Path,
    workspace_roots: &[PathBuf],
    control: &IndexBuildControl,
) -> Result<LoadedAddonIndexResult, String> {
    let dependency_guids =
        read_project_dependency_scope_guids(project_files, storage_root, control)?;
    let workspace_guids = project_files
        .iter()
        .filter_map(|project_file| read_dependency_project_candidate(project_file).ok())
        .map(|candidate| candidate.addon.guid)
        .collect::<BTreeSet<_>>();
    let mut result = load_cached_indexes_from_storage(
        storage_root,
        workspace_roots,
        control,
        false,
        Some(&dependency_guids),
    )?;
    let loaded_guids = result
        .instances
        .iter()
        .map(|instance| instance.guid.to_ascii_uppercase())
        .collect::<BTreeSet<_>>();
    result.missing_instances += dependency_guids
        .iter()
        .filter(|guid| !workspace_guids.contains(*guid) && !loaded_guids.contains(*guid))
        .count();
    Ok(result)
}

/// Resolves the exact cached source instances used by provisional dependency
/// search without decoding their semantic indexes. Resource discovery uses
/// this to enumerate the same add-ons as semantic search.
pub fn read_cached_dependency_addon_sources(
    project_files: &[PathBuf],
    storage_root: &Path,
    control: &IndexBuildControl,
) -> Result<Vec<LoadedAddonSourceInfo>, String> {
    let dependency_guids =
        read_project_dependency_scope_guids(project_files, storage_root, control)?;
    let mut manifests = cached_manifest_descriptors(storage_root, control)?
        .into_iter()
        .map(|(manifest, _)| manifest)
        .filter(|manifest| dependency_guids.contains(&manifest.guid.to_ascii_uppercase()))
        .collect::<Vec<_>>();
    manifests.sort_by(|left, right| {
        left.guid
            .cmp(&right.guid)
            .then_with(|| {
                dependency_source_preference(&left.source_root)
                    .cmp(&dependency_source_preference(&right.source_root))
            })
            .then_with(|| left.source_root.cmp(&right.source_root))
    });
    manifests.dedup_by(|left, right| left.guid.eq_ignore_ascii_case(&right.guid));
    manifests
        .into_iter()
        .map(|manifest| cached_manifest_source_info(&manifest))
        .collect()
}

fn cached_manifest_source_info(
    manifest: &AddonIndexManifestHeader,
) -> Result<LoadedAddonSourceInfo, String> {
    if let Ok(entries) = fs::read_dir(&manifest.source_root) {
        for project_file in entries.flatten().map(|entry| entry.path()).filter(|path| {
            path.extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("gproj"))
        }) {
            if let Ok(candidate) = read_dependency_project_candidate(&project_file) {
                if candidate.addon.guid.eq_ignore_ascii_case(&manifest.guid) {
                    return Ok(LoadedAddonSourceInfo {
                        guid: candidate.addon.guid,
                        display_id: candidate.addon.id,
                        title: candidate.addon.title,
                        source_root: candidate.addon.source_root,
                    });
                }
            }
        }
    }
    let (display_id, title) = manifest
        .display_id
        .strip_suffix(')')
        .and_then(|value| value.split_once(" ("))
        .map(|(id, title)| (id.to_string(), title.to_string()))
        .unwrap_or_else(|| (manifest.display_id.clone(), manifest.display_id.clone()));
    Ok(LoadedAddonSourceInfo {
        guid: manifest.guid.to_ascii_uppercase(),
        display_id,
        title,
        source_root: fs::canonicalize(&manifest.source_root).map_err(|error| {
            format!(
                "Failed to resolve cached add-on source {}: {error}",
                manifest.source_root.display()
            )
        })?,
    })
}

/// Loads the offline project dependency scope from its caches, building only
/// when that scope has no usable published snapshot yet. The Workbench graph
/// is deliberately not involved here: the opened project, Workbench's
/// project-list registry, and unambiguous installed-game path are the offline
/// source description. A later Workbench reconciliation may replace this
/// provisional scope.
pub fn load_or_build_dependency_addon_indexes(
    project_files: &[PathBuf],
    workbench_profile: Option<&Path>,
    storage_root: &Path,
    workspace_roots: &[PathBuf],
    control: &IndexBuildControl,
) -> Result<LoadedAddonIndexResult, String> {
    let cached = load_cached_dependency_addon_indexes(
        project_files,
        storage_root,
        workspace_roots,
        control,
    )?;
    if cached.loaded_instances > 0 && cached.missing_instances == 0 {
        return Ok(cached);
    }

    let graph_start = Instant::now();
    let graph = read_project_dependency_graph(project_files, workbench_profile, control)?;
    let graph_read = graph_start.elapsed();
    load_or_build_addon_indexes(
        graph,
        graph_read,
        storage_root,
        workspace_roots,
        control,
        AddonScopeAuthority::ProjectDependencies,
    )
}

fn cached_manifest_descriptors(
    storage_root: &Path,
    control: &IndexBuildControl,
) -> Result<Vec<(AddonIndexManifestHeader, PathBuf)>, String> {
    if !storage_root.is_dir() {
        return Ok(Vec::new());
    }
    if let Some(catalogue) = read_cache_catalogue(storage_root) {
        let descriptors = catalogue
            .entries
            .into_iter()
            .map(|manifest| {
                let cache_root =
                    storage_root.join(addon_instance_key(&manifest.guid, &manifest.source_root));
                (manifest, cache_root.join("symbols.bin"))
            })
            .collect::<Vec<_>>();
        if descriptors
            .iter()
            .all(|(_, cache_path)| cache_path.is_file())
        {
            return Ok(descriptors);
        }
    }

    let descriptors = scan_cached_manifest_descriptors(storage_root)?;
    control.check()?;
    let _ = write_cache_catalogue(
        storage_root,
        descriptors.iter().map(|(manifest, _)| manifest.clone()),
    );
    Ok(descriptors)
}

fn scan_cached_manifest_descriptors(
    storage_root: &Path,
) -> Result<Vec<(AddonIndexManifestHeader, PathBuf)>, String> {
    let entries = match fs::read_dir(storage_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(format!(
                "Failed to read add-on index storage {}: {error}",
                storage_root.display()
            ))
        }
    };
    let mut descriptors = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        if !entry
            .file_type()
            .map_err(|error| error.to_string())?
            .is_dir()
            || !is_addon_instance_key(&entry.file_name().to_string_lossy())
        {
            continue;
        }
        let cache_root = entry.path();
        let manifest_bytes = match fs::read(cache_root.join(ADDON_MANIFEST_HEADER_FILE)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::read(cache_root.join("manifest.json")) {
                    Ok(bytes) => bytes,
                    Err(_) => continue,
                }
            }
            Err(_) => continue,
        };
        let Ok(manifest) = serde_json::from_slice::<AddonIndexManifestHeader>(&manifest_bytes)
            .or_else(|_| {
                serde_json::from_slice::<AddonIndexManifest>(&manifest_bytes)
                    .map(|manifest| manifest.header())
            })
        else {
            continue;
        };
        if manifest.schema != ADDON_MANIFEST_SCHEMA || manifest.index_file != "symbols.bin" {
            continue;
        }
        descriptors.push((manifest, cache_root.join("symbols.bin")));
    }
    descriptors.sort_by(|(left, _), (right, _)| {
        (&left.guid, &left.source_root, &left.display_id).cmp(&(
            &right.guid,
            &right.source_root,
            &right.display_id,
        ))
    });
    Ok(descriptors)
}

fn read_cache_catalogue(storage_root: &Path) -> Option<AddonCacheCatalogue> {
    let bytes = fs::read(storage_root.join(ADDON_CACHE_CATALOGUE_FILE)).ok()?;
    let catalogue = serde_json::from_slice::<AddonCacheCatalogue>(&bytes).ok()?;
    (catalogue.schema == "reforger-addon-cache-catalogue-v1").then_some(catalogue)
}

fn write_cache_catalogue(
    storage_root: &Path,
    entries: impl IntoIterator<Item = AddonIndexManifestHeader>,
) -> Result<(), String> {
    let mut entries = entries.into_iter().collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        (&left.guid, &left.source_root, &left.display_id).cmp(&(
            &right.guid,
            &right.source_root,
            &right.display_id,
        ))
    });
    entries.dedup_by(|left, right| {
        left.guid.eq_ignore_ascii_case(&right.guid) && left.source_root == right.source_root
    });
    write_json_atomic(
        &storage_root.join(ADDON_CACHE_CATALOGUE_FILE),
        &AddonCacheCatalogue {
            schema: "reforger-addon-cache-catalogue-v1".to_string(),
            entries,
        },
    )?;
    Ok(())
}

fn refresh_cache_catalogue(storage_root: &Path) -> Result<(), String> {
    if !storage_root.is_dir() {
        return Ok(());
    }
    let descriptors = scan_cached_manifest_descriptors(storage_root)?;
    write_cache_catalogue(
        storage_root,
        descriptors.into_iter().map(|(manifest, _)| manifest),
    )
}

fn load_cached_indexes_from_storage(
    storage_root: &Path,
    workspace_roots: &[PathBuf],
    control: &IndexBuildControl,
    base_game_only: bool,
    dependency_guids: Option<&BTreeSet<String>>,
) -> Result<LoadedAddonIndexResult, String> {
    let total_start = Instant::now();
    let graph_start = Instant::now();
    let workspace_roots = workspace_roots
        .iter()
        .filter_map(|root| fs::canonicalize(root).ok())
        .collect::<Vec<_>>();
    let workspace_root_resolution = graph_start.elapsed();
    if !storage_root.is_dir() {
        return Ok(empty_cached_index_result(
            total_start.elapsed(),
            workspace_root_resolution,
            dependency_guids.is_some(),
        ));
    }
    let mut descriptors = Vec::new();
    for (manifest, cache_path) in cached_manifest_descriptors(storage_root, control)? {
        control.check()?;
        if manifest.schema != ADDON_MANIFEST_SCHEMA
            || manifest.index_file != "symbols.bin"
            || (base_game_only && !is_base_game_manifest(&manifest))
            || dependency_guids
                .is_some_and(|guids| !guids.contains(&manifest.guid.to_ascii_uppercase()))
            || workspace_roots
                .iter()
                .any(|root| root.starts_with(&manifest.source_root))
        {
            continue;
        }
        descriptors.push((manifest, cache_path));
    }
    if dependency_guids.is_some() {
        descriptors.sort_by(|(left, _), (right, _)| {
            left.guid
                .cmp(&right.guid)
                .then_with(|| {
                    dependency_source_preference(&left.source_root)
                        .cmp(&dependency_source_preference(&right.source_root))
                })
                .then_with(|| left.source_root.cmp(&right.source_root))
        });
        descriptors.dedup_by(|left, right| left.0.guid.eq_ignore_ascii_case(&right.0.guid));
    }
    descriptors.sort_by(|(left, _), (right, _)| {
        (&left.guid, &left.source_root, &left.display_id).cmp(&(
            &right.guid,
            &right.source_root,
            &right.display_id,
        ))
    });

    let cache_load_start = Instant::now();
    let descriptors = descriptors
        .into_iter()
        .enumerate()
        .map(
            |(sequence, (manifest, cache_path))| CachedManifestDescriptor {
                sequence,
                manifest,
                cache_path,
            },
        )
        .collect::<Vec<_>>();
    let descriptor_count = descriptors.len();
    let descriptors = Arc::new(Mutex::new(descriptors));
    let (cache_sender, cache_receiver) = mpsc::channel();
    let cache_worker_count = addon_index_worker_count(storage_root, descriptor_count)?;
    let mut cache_workers = Vec::with_capacity(cache_worker_count);
    for _ in 0..cache_worker_count {
        let descriptors = descriptors.clone();
        let cache_sender = cache_sender.clone();
        let control = control.clone();
        cache_workers.push(thread::spawn(move || loop {
            let Some(descriptor) = descriptors.lock().unwrap().pop() else {
                return;
            };
            let result = control.check().and_then(|()| {
                let cached =
                    match load_game_data_index_cache_with_control(&descriptor.cache_path, &control)
                    {
                        Ok(cached) => cached,
                        Err(error) if control.is_cancelled() => return Err(error),
                        Err(_) => None,
                    };
                let Some(cached) = cached.filter(|cached| {
                    matches!(
                        &cached.fingerprint,
                        SourceFingerprint::Addon { guid, .. }
                            if guid.eq_ignore_ascii_case(&descriptor.manifest.guid)
                    )
                }) else {
                    return Ok(None);
                };
                if let Some(cache_root) = descriptor.cache_path.parent() {
                    register_cached_source_revision_root(
                        &descriptor.manifest.guid,
                        &descriptor.manifest.revision,
                        cache_root,
                    );
                }
                Ok(Some(cached))
            });
            let _ = cache_sender.send(CompletedCachedManifestLoad {
                sequence: descriptor.sequence,
                manifest: descriptor.manifest,
                cache_path: descriptor.cache_path,
                result,
            });
        }));
    }
    drop(cache_sender);
    let mut completed = Vec::with_capacity(descriptor_count);
    for _ in 0..descriptor_count {
        completed.push(
            cache_receiver
                .recv()
                .map_err(|error| format!("Add-on cache load worker ended unexpectedly: {error}"))?,
        );
    }
    for worker in cache_workers {
        worker
            .join()
            .map_err(|_| "Add-on cache load worker panicked".to_string())?;
    }
    completed.sort_by_key(|completed| completed.sequence);
    let mut summary = RuntimeIndexSummary::default();
    let mut loaded_instances = 0;
    let mut missing_instances = 0;
    let workspace_excluded_instances = 0;
    let mut indexes = Vec::with_capacity(completed.len());
    let mut source_line_starts = BTreeMap::new();
    let mut instances = Vec::with_capacity(completed.len());
    let mut unavailable_instances = Vec::new();
    let mut scope_instances = Vec::with_capacity(completed.len());
    let mut file_start = 0;
    for completed in completed {
        control.check()?;
        let manifest = completed.manifest;
        let cache_path = completed.cache_path;
        let Some(result) = completed.result? else {
            missing_instances += 1;
            unavailable_instances.push(UnavailableAddonIndexInstance {
                guid: manifest.guid,
                display_id: manifest.display_id.clone(),
                title: manifest.display_id,
            });
            continue;
        };
        let (pack_count, script_count, revision) = match &result.fingerprint {
            SourceFingerprint::Addon {
                pack_count,
                catalogue_entry_count,
                artifact_digest,
                ..
            } => (*pack_count, *catalogue_entry_count, artifact_digest.clone()),
            _ => continue,
        };
        let file_count = result.index.files().len();
        source_line_starts.extend(
            result
                .source_line_starts
                .iter()
                .map(|(file, starts)| (SourceFileId(file.0 + file_start), starts.clone())),
        );
        loaded_instances += 1;
        summary.files += result.summary.files;
        summary.bytes += result.summary.bytes;
        summary.indexed_symbols += result.summary.indexed_symbols;
        summary.parse_diagnostics += result.summary.parse_diagnostics;
        summary.lossy_files += result.summary.lossy_files;
        instances.push(LoadedAddonIndexInstance {
            guid: manifest.guid.clone(),
            display_id: manifest.display_id.clone(),
            title: manifest.display_id.clone(),
            thumbnail_color: manifest.thumbnail_color.clone(),
            pack_count,
            script_count,
            file_start,
            file_count,
            cache_path,
            revision,
            cache_status: "cached-only".to_string(),
            cache_detail: Some("source-validation-skipped".to_string()),
            summary: result.summary.clone(),
            timings: result.timings,
            cache_file_bytes: result.cache_file_bytes,
        });
        scope_instances.push(manifest_scope_identity(&manifest));
        indexes.push(result.index);
        file_start += file_count;
    }
    let (index, layer_timings) = SymbolIndex::layered_with_timings(indexes);
    Ok(LoadedAddonIndexResult {
        index: Arc::new(index),
        source_line_starts,
        scope_authority: if dependency_guids.is_some() {
            AddonScopeAuthority::ProjectDependencies
        } else {
            AddonScopeAuthority::WorkbenchLoaded
        },
        summary,
        rebuilt_instances: 0,
        loaded_instances,
        missing_instances,
        workspace_excluded_instances,
        timings: LoadedAddonIndexTimings {
            graph_read: total_start.elapsed(),
            workspace_root_resolution,
            cache_prune: Duration::ZERO,
            cache_metadata_read: Duration::ZERO,
            source_inspection: Duration::ZERO,
            index_load_or_build: cache_load_start.elapsed(),
            layer_rebase: layer_timings.rebase,
            layer_file_projection: layer_timings.file_projection,
            layer_lookup_projection: layer_timings.lookup_projection,
            layer_compose: layer_timings.total,
            total: total_start.elapsed(),
        },
        instances,
        unavailable_instances,
        scope_instances,
    })
}

pub fn loaded_workbench_graph_matches_scope(
    inventory_path: &Path,
    workspace_roots: &[PathBuf],
    scope_instances: &[LoadedAddonInstanceIdentity],
) -> Result<bool, String> {
    let graph = read_loaded_addon_graph(inventory_path)?;
    let workspace_roots = workspace_roots
        .iter()
        .filter_map(|root| fs::canonicalize(root).ok())
        .collect::<Vec<_>>();
    let graph_scope = graph
        .addons
        .into_iter()
        .filter(|addon| {
            !workspace_roots
                .iter()
                .any(|root| root.starts_with(&addon.source_root))
        })
        .map(|addon| LoadedAddonInstanceIdentity {
            guid: addon.guid,
            source_root: addon.source_root,
        })
        .collect::<Vec<_>>();
    Ok(graph_scope == scope_instances)
}

fn addon_scope_identity(addon: &LoadedAddonSource) -> LoadedAddonInstanceIdentity {
    LoadedAddonInstanceIdentity {
        guid: addon.guid.clone(),
        source_root: addon.source_root.clone(),
    }
}

fn manifest_scope_identity(manifest: &AddonIndexManifestHeader) -> LoadedAddonInstanceIdentity {
    LoadedAddonInstanceIdentity {
        guid: manifest.guid.clone(),
        source_root: manifest.source_root.clone(),
    }
}

fn register_cached_source_revision(
    cache_root: &Path,
    header: &AddonIndexManifestHeader,
) -> Result<(), String> {
    let manifest_path = cache_root.join("manifest.json");
    let manifest = serde_json::from_slice::<AddonIndexManifest>(
        &fs::read(&manifest_path).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    if manifest.guid != header.guid
        || manifest.revision != header.revision
        || manifest.source_root != header.source_root
        || manifest.pack_artifacts != header.pack_artifacts
    {
        return Err("Cached add-on source metadata does not match its header".to_string());
    }

    register_cached_source_revision_from_locators(header, manifest.scripts)
}

fn register_cached_source_revision_from_locators(
    header: &AddonIndexManifestHeader,
    scripts: Vec<ScriptLocator>,
) -> Result<(), String> {
    if scripts.len() > header.script_count {
        return Err(format!(
            "Cached locator count {} exceeds manifest script count {}",
            scripts.len(),
            header.script_count
        ));
    }
    let pack_paths = header
        .pack_artifacts
        .iter()
        .map(|artifact| artifact.relative_path.as_str())
        .collect::<BTreeSet<_>>();
    let mut logical_paths = BTreeSet::new();
    for script in &scripts {
        if script.logical_path.is_empty() || !logical_paths.insert(&script.logical_path) {
            return Err(
                "Cached locator table contains a duplicate or empty logical path".to_string(),
            );
        }
        if !pack_paths.contains(script.pack_relative_path.as_str()) {
            return Err(format!(
                "Cached locator references an unknown pack artifact {}",
                script.pack_relative_path
            ));
        }
    }
    let artifacts = header
        .pack_artifacts
        .iter()
        .map(|artifact| ArtifactStamp {
            path: header.source_root.join(&artifact.relative_path),
            bytes: artifact.bytes,
            modified_unix_ms: artifact.modified_unix_ms,
        })
        .collect();
    let mut entries = scripts
        .into_iter()
        .map(|script| {
            Ok(PackedSourceEntry {
                entry: PakEntry::from_locator(
                    script.logical_path,
                    script.offset,
                    script.compressed_length,
                    script.original_length,
                    script.compression,
                    header.source_root.join(script.pack_relative_path),
                ),
                compressed_payload_sha256: script.compressed_payload_sha256,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    entries.sort_by(|left, right| left.entry.logical_path().cmp(right.entry.logical_path()));

    register_source_revision(
        &header.guid,
        &header.revision,
        Arc::new(PackedSourceRevision { artifacts, entries }),
    );
    Ok(())
}

fn dependency_source_preference(source_root: &Path) -> u8 {
    match loose_script_paths(source_root) {
        Ok(paths) if !paths.is_empty() => 0,
        Ok(_) | Err(_) => 1,
    }
}

fn empty_cached_index_result(
    total: Duration,
    workspace_root_resolution: Duration,
    dependency_scope: bool,
) -> LoadedAddonIndexResult {
    LoadedAddonIndexResult {
        index: Arc::new(SymbolIndex::layered(Vec::new())),
        source_line_starts: BTreeMap::new(),
        scope_authority: if dependency_scope {
            AddonScopeAuthority::ProjectDependencies
        } else {
            AddonScopeAuthority::WorkbenchLoaded
        },
        summary: RuntimeIndexSummary::default(),
        rebuilt_instances: 0,
        loaded_instances: 0,
        missing_instances: 0,
        workspace_excluded_instances: 0,
        timings: LoadedAddonIndexTimings {
            workspace_root_resolution,
            total,
            ..Default::default()
        },
        instances: Vec::new(),
        unavailable_instances: Vec::new(),
        scope_instances: Vec::new(),
    }
}

fn is_base_game_addon(addon: &LoadedAddonSource) -> bool {
    addon.guid.eq_ignore_ascii_case(BASE_GAME_GUID)
        || addon.id.eq_ignore_ascii_case("core")
        || addon.title.eq_ignore_ascii_case("core")
}

fn is_base_game_manifest(manifest: &AddonIndexManifestHeader) -> bool {
    manifest.guid.eq_ignore_ascii_case(BASE_GAME_GUID)
        || manifest.display_id.eq_ignore_ascii_case("core")
        || manifest
            .source_root
            .file_name()
            .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("core"))
}

fn addon_index_worker_count(storage_root: &Path, task_count: usize) -> Result<usize, String> {
    if task_count == 0 {
        return Ok(0);
    }
    if !addon_index_storage_is_empty(storage_root)? {
        return Ok(2.min(task_count));
    }
    let logical_cpus = thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1);
    Ok(addon_index_worker_count_for(logical_cpus, task_count))
}

fn addon_index_storage_is_empty(storage_root: &Path) -> Result<bool, String> {
    match fs::read_dir(storage_root) {
        Ok(entries) => Ok(!entries.flatten().any(|entry| {
            entry.file_type().is_ok_and(|kind| {
                kind.is_dir() && is_addon_instance_key(&entry.file_name().to_string_lossy())
            })
        })),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(format!(
            "Failed to inspect add-on index storage {}: {error}",
            storage_root.display()
        )),
    }
}

fn addon_index_worker_count_for(logical_cpus: usize, task_count: usize) -> usize {
    logical_cpus
        .max(1)
        .min(MAX_ADDON_INDEX_WORKERS)
        .min(task_count)
}

fn source_build_worker_count(addon_worker_count: usize) -> usize {
    let logical_cpus = thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1);
    (logical_cpus / addon_worker_count.max(1)).max(1)
}

fn standalone_source_build_worker_count() -> usize {
    source_build_worker_count(1)
}

fn addon_inspection_worker_count(task_count: usize) -> usize {
    if task_count == 0 {
        return 0;
    }
    let logical_cpus = thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1);
    addon_index_worker_count_for(logical_cpus, task_count)
}

/// Removes cache roots which are not physical instances in the current
/// Workbench graph. A GUID can move from an unpacked workspace copy to a
/// packed copy (or vice versa), but only the root currently selected by
/// Workbench may remain persisted.
fn prune_unloaded_addon_caches(
    storage_root: &Path,
    loaded_addons: &[LoadedAddonSource],
) -> Result<(), String> {
    let active = loaded_addons
        .iter()
        .map(|addon| addon_instance_key(&addon.guid, &addon.source_root))
        .collect::<HashSet<_>>();
    let entries = match fs::read_dir(storage_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "Failed to read add-on index storage {}: {error}",
                storage_root.display()
            ))
        }
    };
    for entry in entries {
        let entry = entry.map_err(|error| error.to_string())?;
        let key = entry.file_name();
        let key = key.to_string_lossy();
        if !active.contains(key.as_ref())
            && is_addon_instance_key(&key)
            && entry
                .file_type()
                .map_err(|error| error.to_string())?
                .is_dir()
        {
            fs::remove_dir_all(entry.path()).map_err(|error| {
                format!(
                    "Failed to remove inactive add-on cache {}: {error}",
                    entry.path().display()
                )
            })?;
        }
    }
    Ok(())
}

fn is_addon_instance_key(value: &str) -> bool {
    let Some((guid, digest)) = value.split_once('-') else {
        return false;
    };
    guid.len() == 16
        && guid.bytes().all(|byte| byte.is_ascii_hexdigit())
        && digest.len() == 64
        && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn manifest_matches_current_source(
    manifest: &AddonIndexManifestHeader,
    cache_schema: &str,
    cache_format_version: u32,
    cache_index_shape: &str,
    guid: &str,
    display_id: &str,
    source_root: &Path,
    revision: &str,
    pack_count: usize,
    script_count: usize,
    pack_artifacts: &[PackArtifact],
    index_bytes: u64,
) -> bool {
    manifest.schema == ADDON_MANIFEST_SCHEMA
        && manifest.cache_schema == cache_schema
        && manifest.cache_format_version == cache_format_version
        && manifest.cache_index_shape == cache_index_shape
        && manifest.extractor_schema == "pac1-selected-script-payload-v2"
        && manifest.guid == guid
        && manifest.display_id == display_id
        && manifest.source_root == source_root
        && manifest.source_precedence == "Workbench loaded add-on order"
        && manifest.revision == revision
        && manifest.pack_count == pack_count
        && manifest.script_count == script_count
        && manifest.pack_artifacts == pack_artifacts
        && manifest.index_file == "symbols.bin"
        && manifest.index_bytes == index_bytes
}

fn cached_manifest_matches_inspection(
    inspection: &BaseGameInspection,
    storage_root: &Path,
) -> Result<bool, String> {
    let (pack_count, script_count) = match &inspection.fingerprint {
        SourceFingerprint::Addon {
            pack_count,
            catalogue_entry_count,
            ..
        } => (*pack_count, *catalogue_entry_count),
        _ => return Ok(false),
    };
    let addon_root = storage_root.join(addon_instance_key(&inspection.guid, &inspection.root));
    let cache = addon_root.join("symbols.bin");
    let manifest_path = addon_root.join("manifest.json");
    let manifest_header_path = addon_root.join(ADDON_MANIFEST_HEADER_FILE);
    let cache_bytes = cache.metadata().map(|metadata| metadata.len()).unwrap_or(0);
    let (cache_schema, cache_format_version, cache_index_shape) = cache_format_identity();
    let manifest_header = match fs::read(&manifest_header_path) {
        Ok(bytes) => serde_json::from_slice::<AddonIndexManifestHeader>(&bytes)
            .ok()
            .map(|manifest| (manifest, true)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::read(&manifest_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<AddonIndexManifest>(&bytes).ok())
            .map(|manifest| (manifest.header(), false)),
        Err(_) => None,
    };
    Ok(manifest_header.is_some_and(|(manifest, compact_header)| {
        manifest_matches_current_source(
            &manifest,
            cache_schema,
            cache_format_version,
            cache_index_shape,
            &inspection.guid,
            &inspection.display_id,
            &inspection.root,
            &inspection.artifact_digest,
            pack_count,
            script_count,
            &inspection.artifacts,
            cache_bytes,
        ) && (!compact_header
            || match manifest.manifest_sha256.as_deref() {
                Some(expected) => {
                    fs::read(&manifest_path).is_ok_and(|bytes| sha256_hex(&bytes) == expected)
                }
                None => false,
            })
    }))
}

fn load_or_build_inspected_addon(
    inspection: BaseGameInspection,
    storage_root: &Path,
    control: &IndexBuildControl,
    inspection_elapsed: std::time::Duration,
    source_build_worker_count: usize,
) -> Result<GameDataIndexCacheResult, String> {
    let source_root = inspection.root.clone();
    let thumbnail_color = inspection.thumbnail_color.clone();
    let addon_guid = inspection.guid.clone();
    let addon_display_id = inspection.display_id.clone();
    let pack_artifacts = inspection.artifacts.clone();
    let scripts = inspection.scripts.clone();
    let fingerprint = inspection.fingerprint.clone();
    let artifact_digest = inspection.artifact_digest.clone();
    let (pack_count, script_count) = match &fingerprint {
        SourceFingerprint::Addon {
            pack_count,
            catalogue_entry_count,
            ..
        } => (*pack_count, *catalogue_entry_count),
        _ => unreachable!("base-game PAC inspection always creates an add-on fingerprint"),
    };
    let instance_key = addon_instance_key(&inspection.guid, &source_root);
    let addon_root = storage_root.join(&instance_key);
    discard_legacy_addon_cache_layout(&addon_root)?;
    let cache = addon_root.join("symbols.bin");
    let manifest_path = addon_root.join("manifest.json");
    let manifest_header_path = addon_root.join(ADDON_MANIFEST_HEADER_FILE);
    let (cache_schema, cache_format_version, cache_index_shape) = cache_format_identity();
    let cache_metadata_read_start = Instant::now();
    let manifest_reusable = cached_manifest_matches_inspection(&inspection, storage_root)?;
    let cache_metadata_read = cache_metadata_read_start.elapsed();
    let rebuild_reason = if manifest_reusable {
        "cache-missing-invalid-or-source-changed"
    } else if cache.is_file() {
        "manifest-mismatch"
    } else {
        "cache-missing"
    };
    let source_revision = packed_source_revision(&inspection);
    let build_sources = source_revision.clone();
    let mut result = load_or_build_archive_index_with_reuse_and_locator(
        &cache,
        fingerprint,
        artifact_digest.clone(),
        manifest_reusable,
        rebuild_reason,
        || encode_locator_table(&scripts).map(Some),
        || {
            build_inspected_base_game(
                inspection,
                &build_sources,
                control,
                source_build_worker_count,
            )
        },
    )?;
    result.timings.fingerprint = inspection_elapsed;
    result.timings.cache_metadata_read = cache_metadata_read;
    result.timings.total += inspection_elapsed;
    result.timings.total += cache_metadata_read;
    if matches!(result.cache_status, IndexCacheStatus::Rebuilt { .. }) {
        let cache_metadata_publish_start = Instant::now();
        control.check()?;
        let manifest_scripts = scripts
            .into_iter()
            .map(|mut script| {
                script.uri =
                    virtual_source_uri(&addon_guid, &artifact_digest, &script.logical_path)?;
                Ok(script)
            })
            .collect::<Result<Vec<_>, String>>()?;
        let manifest = AddonIndexManifest {
            schema: ADDON_MANIFEST_SCHEMA.to_string(),
            cache_schema: cache_schema.to_string(),
            cache_format_version,
            cache_index_shape: cache_index_shape.to_string(),
            extractor_schema: "pac1-selected-script-payload-v2".to_string(),
            guid: addon_guid.clone(),
            display_id: addon_display_id,
            thumbnail_color,
            source_root,
            source_precedence: "Workbench loaded add-on order".to_string(),
            revision: artifact_digest.clone(),
            pack_count,
            script_count,
            pack_artifacts,
            scripts: manifest_scripts,
            index_file: "symbols.bin".to_string(),
            index_bytes: result.cache_file_bytes.unwrap_or(0),
        };
        let manifest_bytes = write_json_atomic(&manifest_path, &manifest)?;
        let mut manifest_header = manifest.header();
        manifest_header.manifest_sha256 = Some(sha256_hex(&manifest_bytes));
        write_json_atomic(&manifest_header_path, &manifest_header)?;
        result.timings.cache_metadata_publish = cache_metadata_publish_start.elapsed();
        result.timings.total += result.timings.cache_metadata_publish;
    }
    register_source_revision(&addon_guid, &artifact_digest, source_revision);
    Ok(result)
}

/// The retired pointer/revision layout is deliberately not read or migrated.
/// A flattened cache is rebuilt from the authoritative Workbench graph.
fn discard_legacy_addon_cache_layout(addon_root: &Path) -> Result<(), String> {
    if addon_root.join("current.json").is_file() || addon_root.join("revisions").is_dir() {
        fs::remove_dir_all(addon_root)
            .map_err(|error| format!("Failed to remove retired add-on cache layout: {error}"))?;
    }
    Ok(())
}

/// Workspace scripts are live inputs, so their matching Workbench instance
/// must never retain a packed cache that can shadow or duplicate that source.
fn remove_workspace_addon_cache(
    storage_root: &Path,
    addon: &LoadedAddonSource,
) -> Result<(), String> {
    let addon_root = storage_root.join(addon_instance_key(&addon.guid, &addon.source_root));
    match fs::remove_dir_all(&addon_root) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "Failed to remove obsolete workspace add-on cache {}: {error}",
            addon_root.display()
        )),
    }
}

fn loose_script_paths(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut paths = Vec::new();
    let entries = fs::read_dir(root).map_err(|error| {
        format!(
            "Failed to read Workbench add-on root {}: {error}",
            root.display()
        )
    })?;
    for entry in entries {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.is_dir()
            && path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("scripts"))
        {
            collect_loose_script_paths(&path, &mut paths)?;
        }
    }
    paths.sort();
    Ok(paths)
}

fn collect_loose_script_paths(root: &Path, paths: &mut Vec<PathBuf>) -> Result<(), String> {
    for entry in fs::read_dir(root).map_err(|error| {
        format!(
            "Failed to read Workbench add-on root {}: {error}",
            root.display()
        )
    })? {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.is_dir() {
            collect_loose_script_paths(&path, paths)?;
        } else if path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("c"))
        {
            paths.push(path);
        }
    }
    Ok(())
}

pub fn publish_inventory_addon_manifests_from_path(
    inventory_path: &Path,
    storage_root: &Path,
    control: &IndexBuildControl,
) -> Result<(), String> {
    control.check()?;
    let inventory = read_inventory(inventory_path)?;
    publish_inventory_addon_manifests(&inventory, storage_root, control)
}

/// Resolves one immutable virtual document from its PAC catalogue entry. Only
/// the requested source payload is decoded.
pub fn read_virtual_source(uri: &str) -> Result<String, String> {
    let parsed = Url::parse(uri).map_err(|error| format!("Invalid pack source URI: {error}"))?;
    if parsed.scheme() != VIRTUAL_SOURCE_SCHEME {
        return Err(format!(
            "Unsupported source URI scheme '{}'",
            parsed.scheme()
        ));
    }
    let guid = parsed
        .host_str()
        .ok_or_else(|| "Pack source URI has no add-on GUID".to_string())?
        .to_ascii_uppercase();
    let mut path = parsed.path().trim_start_matches('/').splitn(2, '/');
    let revision = path
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Pack source URI has no revision".to_string())?;
    let logical_path = path
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Pack source URI has no logical path".to_string())?;
    // VS Code normalizes custom URI authorities (including the GUID's case).
    // The registry is keyed by the canonical identity emitted during indexing,
    // never by a client-provided serialization.
    let key = revision_key(&guid, revision);
    let sources = SOURCE_REVISIONS
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| "Packed source revisions are unavailable".to_string())?
        .get(&key)
        .cloned();
    if sources.is_none() {
        load_cached_source_revision(&key)?;
    }
    let sources = sources
        .or_else(|| {
            SOURCE_REVISIONS
                .get_or_init(Default::default)
                .lock()
                .ok()
                .and_then(|revisions| revisions.get(&key).cloned())
        })
        .ok_or_else(|| format!("Add-on {guid} revision {revision} is not loaded"))?;
    sources.validate_artifacts()?;
    let source_entry = sources
        .source_entry(logical_path)
        .ok_or_else(|| format!("Pack source does not exist: {logical_path}"))?;
    let entry = &source_entry.entry;
    let mut bytes = Vec::with_capacity(entry.original_length() as usize);
    PakReader::open(entry.archive_path())
        .map_err(|error| error.to_string())?
        .read_verified_to_with_cancel(
            entry,
            &source_entry.compressed_payload_sha256,
            &mut bytes,
            || false,
        )
        .map_err(|error| error.to_string())?;
    Ok(decode_source_bytes(bytes))
}

/// Reads a virtual source document from the immutable cache that published its
/// index. MCP starts in a separate process from the language server, so it
/// must register the cache root before resolving a `reforger-pak` URI.
pub fn read_cached_virtual_source(uri: &str, cache_path: &Path) -> Result<String, String> {
    let parsed = Url::parse(uri).map_err(|error| format!("Invalid pack source URI: {error}"))?;
    if parsed.scheme() != VIRTUAL_SOURCE_SCHEME {
        return Err(format!(
            "Unsupported source URI scheme '{}'",
            parsed.scheme()
        ));
    }
    let guid = parsed
        .host_str()
        .ok_or_else(|| "Pack source URI has no add-on GUID".to_string())?;
    let revision = parsed
        .path()
        .trim_start_matches('/')
        .split('/')
        .next()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Pack source URI has no revision".to_string())?;
    let cache_root = cache_path.parent().unwrap_or_else(|| Path::new("."));
    register_cached_source_revision_root(guid, revision, cache_root);
    read_virtual_source(uri)
}

#[derive(Debug)]
pub struct CachedVirtualSourceBatch {
    pub sources: Vec<Result<String, String>>,
    pub revisions_validated: usize,
    pub archives_opened: usize,
}

#[derive(Debug)]
struct CachedVirtualSourceRequest {
    output_index: usize,
    logical_path: String,
}

#[derive(Debug)]
struct CachedVirtualSourceRevisionRequest {
    guid: String,
    revision: String,
    sources: Vec<CachedVirtualSourceRequest>,
}

/// Reads many immutable virtual documents while validating each source
/// revision once and opening each referenced PAC archive once. Results remain
/// aligned with `uris`, so one unreadable source does not discard the rest.
pub fn read_cached_virtual_sources(
    uris: &[String],
    cache_path: &Path,
    control: &IndexBuildControl,
) -> Result<CachedVirtualSourceBatch, String> {
    let mut results = uris
        .iter()
        .map(|_| Err("Packed source was not read".to_string()))
        .collect::<Vec<_>>();
    let mut revisions = BTreeMap::<String, CachedVirtualSourceRevisionRequest>::new();
    for (output_index, uri) in uris.iter().enumerate() {
        control.check()?;
        let parsed = match Url::parse(uri) {
            Ok(parsed) => parsed,
            Err(error) => {
                results[output_index] = Err(format!("Invalid pack source URI: {error}"));
                continue;
            }
        };
        if parsed.scheme() != VIRTUAL_SOURCE_SCHEME {
            results[output_index] = Err(format!(
                "Unsupported source URI scheme '{}'",
                parsed.scheme()
            ));
            continue;
        }
        let Some(guid) = parsed.host_str() else {
            results[output_index] = Err("Pack source URI has no add-on GUID".to_string());
            continue;
        };
        let mut path = parsed.path().trim_start_matches('/').splitn(2, '/');
        let Some(revision) = path.next().filter(|value| !value.is_empty()) else {
            results[output_index] = Err("Pack source URI has no revision".to_string());
            continue;
        };
        let Some(logical_path) = path.next().filter(|value| !value.is_empty()) else {
            results[output_index] = Err("Pack source URI has no logical path".to_string());
            continue;
        };
        let guid = guid.to_ascii_uppercase();
        let key = revision_key(&guid, revision);
        let request = revisions
            .entry(key)
            .or_insert_with(|| CachedVirtualSourceRevisionRequest {
                guid,
                revision: revision.to_string(),
                sources: Vec::new(),
            });
        request.sources.push(CachedVirtualSourceRequest {
            output_index,
            logical_path: logical_path.to_string(),
        });
    }

    let cache_root = cache_path.parent().unwrap_or_else(|| Path::new("."));
    let mut revisions_validated = 0;
    let mut archives_opened = 0;
    for (key, request) in revisions {
        control.check()?;
        register_cached_source_revision_root(&request.guid, &request.revision, cache_root);
        let source_revision = match loaded_source_revision(&key) {
            Ok(source_revision) => source_revision,
            Err(error) => {
                set_batch_source_errors(&mut results, &request.sources, &error);
                continue;
            }
        };
        if let Err(error) = source_revision.validate_artifacts() {
            set_batch_source_errors(&mut results, &request.sources, &error);
            continue;
        }
        revisions_validated += 1;

        let mut archives = BTreeMap::<PathBuf, Vec<(usize, PakEntry, String)>>::new();
        for source in &request.sources {
            let Some(entry) = source_revision.source_entry(&source.logical_path) else {
                results[source.output_index] = Err(format!(
                    "Pack source does not exist: {}",
                    source.logical_path
                ));
                continue;
            };
            archives
                .entry(entry.entry.archive_path().to_path_buf())
                .or_default()
                .push((
                    source.output_index,
                    entry.entry.clone(),
                    entry.compressed_payload_sha256.clone(),
                ));
        }

        for (archive_path, mut entries) in archives {
            control.check()?;
            let mut reader = match PakReader::open(&archive_path) {
                Ok(reader) => reader,
                Err(error) => {
                    set_archive_source_errors(&mut results, &entries, &error.to_string());
                    continue;
                }
            };
            archives_opened += 1;
            entries.sort_by_key(|(_, entry, _)| entry.offset());
            for (output_index, entry, compressed_payload_sha256) in entries {
                control.check()?;
                let mut bytes = Vec::with_capacity(entry.original_length() as usize);
                if let Err(error) = reader.read_verified_to_with_cancel(
                    &entry,
                    &compressed_payload_sha256,
                    &mut bytes,
                    || control.is_cancelled(),
                ) {
                    control.check()?;
                    results[output_index] = Err(error.to_string());
                    continue;
                }
                results[output_index] = Ok(decode_source_bytes(bytes));
            }
        }
    }

    Ok(CachedVirtualSourceBatch {
        sources: results,
        revisions_validated,
        archives_opened,
    })
}

fn decode_source_bytes(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(source) => source,
        Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
    }
}

fn loaded_source_revision(key: &str) -> Result<Arc<PackedSourceRevision>, String> {
    let sources = SOURCE_REVISIONS
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| "Packed source revisions are unavailable".to_string())?
        .get(key)
        .cloned();
    if sources.is_none() {
        load_cached_source_revision(key)?;
    }
    sources
        .or_else(|| {
            SOURCE_REVISIONS
                .get_or_init(Default::default)
                .lock()
                .ok()
                .and_then(|revisions| revisions.get(key).cloned())
        })
        .ok_or_else(|| format!("Packed source revision {key} is not loaded"))
}

fn set_batch_source_errors(
    results: &mut [Result<String, String>],
    requests: &[CachedVirtualSourceRequest],
    error: &str,
) {
    for request in requests {
        results[request.output_index] = Err(error.to_string());
    }
}

fn set_archive_source_errors(
    results: &mut [Result<String, String>],
    entries: &[(usize, PakEntry, String)],
    error: &str,
) {
    for (output_index, _, _) in entries {
        results[*output_index] = Err(error.to_string());
    }
}

impl PackedSourceRevision {
    fn source_entry(&self, logical_path: &str) -> Option<&PackedSourceEntry> {
        self.entries
            .binary_search_by(|entry| entry.entry.logical_path().cmp(logical_path))
            .ok()
            .map(|index| &self.entries[index])
    }

    fn validate_artifacts(&self) -> Result<(), String> {
        for expected in &self.artifacts {
            let metadata = fs::metadata(&expected.path)
                .map_err(|error| format!("Packed source revision is unavailable: {error}"))?;
            let modified = modified_unix_ms(&metadata);
            if metadata.len() != expected.bytes || modified != expected.modified_unix_ms {
                return Err(format!(
                    "Packed source revision changed on disk: {}",
                    expected.path.display()
                ));
            }
        }
        Ok(())
    }
}

fn inspect_base_game(
    inventory_path: &Path,
    control: &IndexBuildControl,
) -> Result<BaseGameInspection, String> {
    let root = base_game_root(inventory_path)?;
    inspect_packed_addon(
        BASE_GAME_GUID.to_string(),
        "Arma Reforger base game".to_string(),
        root.clone(),
        base_game_archive_paths(&root).to_vec(),
        control,
    )
}

fn inspect_packed_addon(
    guid: String,
    display_id: String,
    root: PathBuf,
    archive_paths: Vec<PathBuf>,
    control: &IndexBuildControl,
) -> Result<BaseGameInspection, String> {
    let mut hasher = Sha256::new();
    hasher.update(b"reforger-base-pac-catalogue-v2");
    let (cache_schema, cache_version, cache_shape) = cache_format_identity();
    hasher.update(cache_schema.as_bytes());
    hasher.update(cache_version.to_le_bytes());
    hasher.update(cache_shape.as_bytes());
    hasher.update(b"pac1-selected-script-payload-v2");
    // A virtual packed-source revision is also an add-on-instance identity.
    // Two loaded roots can legitimately share GUID and bytes, but their source
    // URIs must still never target the other instance's archive.
    hasher.update(root.to_string_lossy().to_ascii_lowercase().as_bytes());
    let thumbnail_color = addon_thumbnail_color(&root);
    if let Some(color) = &thumbnail_color {
        hasher.update(b"thumbnail-color");
        hasher.update(color.as_bytes());
    }
    let mut archives = Vec::new();
    let mut latest_modified = 0_u128;
    let mut script_count = 0_usize;
    let mut artifacts = Vec::new();
    let mut scripts = Vec::new();
    for archive_path in archive_paths {
        control.check()?;
        let metadata = fs::metadata(&archive_path)
            .map_err(|error| format!("Failed to stat {}: {error}", archive_path.display()))?;
        let modified = modified_unix_ms(&metadata);
        latest_modified = latest_modified.max(modified);
        hasher.update(
            archive_path
                .strip_prefix(&root)
                .unwrap_or(&archive_path)
                .to_string_lossy()
                .replace('\\', "/")
                .as_bytes(),
        );
        hasher.update(metadata.len().to_le_bytes());
        hasher.update(modified.to_le_bytes());
        let archive = PakArchive::inspect_with_cancel(&archive_path, || control.is_cancelled())
            .map_err(|error| format!("Failed to inspect {}: {error}", archive_path.display()))?;
        let entries = archive.select(PakSelection::scripts()).map_err(|error| {
            format!(
                "Failed to select scripts from {}: {error}",
                archive_path.display()
            )
        })?;
        let (selected_payload_sha256, entry_payload_sha256) =
            selected_payload_digest(&archive_path, &entries, control)?;
        hasher.update(selected_payload_sha256.as_bytes());
        let relative_archive = normalized_relative(&root, &archive_path);
        for entry in &entries {
            hasher.update(entry.logical_path().as_bytes());
            hasher.update(entry.offset().to_le_bytes());
            hasher.update(entry.compressed_length().to_le_bytes());
            hasher.update(entry.original_length().to_le_bytes());
            hasher.update(entry.compression().to_le_bytes());
            scripts.push(ScriptLocator {
                uri: String::new(),
                logical_path: entry.logical_path().to_string(),
                pack_relative_path: relative_archive.clone(),
                offset: entry.offset(),
                compressed_length: entry.compressed_length(),
                original_length: entry.original_length(),
                compression: entry.compression(),
                compressed_payload_sha256: entry_payload_sha256
                    .get(entry.logical_path())
                    .cloned()
                    .unwrap_or_default(),
            });
        }
        artifacts.push(PackArtifact {
            relative_path: relative_archive,
            bytes: metadata.len(),
            modified_unix_ms: modified,
            selected_payload_sha256,
            strong_manifest_sha512: adjacent_manifest_sha512(&archive_path),
        });
        script_count += entries.len();
        archives.push((archive, entries));
    }
    let loose_files = loose_script_paths(&root)?;
    for file in &loose_files {
        control.check()?;
        let relative = normalized_relative(&root, file);
        hasher.update(b"loose-script");
        hasher.update(relative.as_bytes());
        hasher.update(
            fs::read(file)
                .map_err(|error| format!("Failed to read {}: {error}", file.display()))?,
        );
    }
    let artifact_digest = format!("{:x}", hasher.finalize());
    Ok(BaseGameInspection {
        guid: guid.clone(),
        display_id,
        root,
        thumbnail_color,
        fingerprint: SourceFingerprint::Addon {
            guid,
            artifact_digest: artifact_digest.clone(),
            pack_count: archives.len(),
            catalogue_entry_count: script_count + loose_files.len(),
        },
        artifact_digest,
        archives,
        loose_files,
        artifacts,
        scripts,
    })
}

fn build_inspected_base_game(
    inspection: BaseGameInspection,
    source_revision: &PackedSourceRevision,
    control: &IndexBuildControl,
    source_build_worker_count: usize,
) -> Result<IndexBuildResult, String> {
    let source_acquisition_start = Instant::now();
    let revision = inspection.artifact_digest.clone();
    let mut logical_paths = BTreeSet::new();
    let mut sources = Vec::new();
    for (archive, entries) in inspection.archives {
        let mut reader = archive.reader().map_err(|error| error.to_string())?;
        for entry in entries {
            control.check()?;
            let logical = entry.logical_path().to_string();
            if !logical_paths.insert(logical.to_ascii_lowercase()) {
                return Err(format!("Duplicate base-game script path: {logical}"));
            }
            let relative = PathBuf::from(&logical);
            let uri = virtual_source_uri(&inspection.guid, &revision, &logical)?;
            let expected = source_revision
                .source_entry(&logical)
                .ok_or_else(|| format!("Missing packed source identity for {logical}"))?;
            let mut bytes = Vec::with_capacity(entry.original_length() as usize);
            reader
                .read_verified_to_with_cancel(
                    &entry,
                    &expected.compressed_payload_sha256,
                    &mut bytes,
                    || control.is_cancelled(),
                )
                .map_err(|error| {
                    format!(
                        "Failed to read {logical} from {}: {error}",
                        archive.path().display()
                    )
                })?;
            sources.push(IndexSourceText {
                display_path: PathBuf::from(&uri),
                bytes,
                metadata: SourceFileMetadata {
                    kind: SourceKind::GameData,
                    category: source_category_for_path(SourceKind::GameData, Some(&relative)),
                    absolute_path: None,
                    virtual_source: Some(VirtualSourceIdentity {
                        uri,
                        addon_guid: inspection.guid.clone(),
                        revision: revision.clone(),
                        logical_path: logical,
                    }),
                    root_path: None,
                    relative_path: Some(relative),
                    priority: SOURCE_PRIORITY_GAME_DATA,
                },
            });
        }
    }
    for file in inspection.loose_files {
        control.check()?;
        let relative = file
            .strip_prefix(&inspection.root)
            .unwrap_or(&file)
            .to_path_buf();
        sources.push(IndexSourceText {
            display_path: file.clone(),
            bytes: fs::read(&file)
                .map_err(|error| format!("Failed to read {}: {error}", file.display()))?,
            metadata: SourceFileMetadata {
                kind: SourceKind::GameData,
                category: source_category_for_path(SourceKind::GameData, Some(&relative)),
                absolute_path: Some(file),
                virtual_source: None,
                root_path: Some(inspection.root.clone()),
                relative_path: Some(relative),
                priority: SOURCE_PRIORITY_GAME_DATA,
            },
        });
    }
    let source_acquisition = source_acquisition_start.elapsed();
    let mut result = build_index_from_sources(sources, control, source_build_worker_count)?;
    result.summary.timings.source_acquisition = source_acquisition;
    result.summary.timings.total += source_acquisition;
    Ok(result)
}

fn virtual_source_uri(guid: &str, revision: &str, logical_path: &str) -> Result<String, String> {
    let base = Url::parse(&format!("{VIRTUAL_SOURCE_SCHEME}://{guid}/{revision}/"))
        .map_err(|error| error.to_string())?;
    base.join(logical_path)
        .map(|uri| uri.to_string())
        .map_err(|error| error.to_string())
}

fn base_game_archive_paths(root: &Path) -> [PathBuf; 2] {
    [
        root.join("data").join("data007.pak"),
        root.join("core").join("data.pak"),
    ]
}

/// The Workbench graph authorizes one exact add-on root. The pack reader only
/// enumerates direct PAC artifacts at that root; it never discovers another
/// add-on folder or substitutes an installed duplicate.
fn addon_archive_paths(root: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = fs::read_dir(root).map_err(|error| {
        format!(
            "Failed to read Workbench add-on root {}: {error}",
            root.display()
        )
    })?;
    let mut archives = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("pak"))
        })
        .collect::<Vec<_>>();
    archives.sort();
    Ok(archives)
}

/// Returns only the direct PAC artifacts Workbench's loaded add-on source
/// route authorizes for this exact source root.
pub fn loaded_addon_archive_paths(root: &Path) -> Result<Vec<PathBuf>, String> {
    addon_archive_paths(root)
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<Vec<u8>, String> {
    let mut json = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    json.push(b'\n');
    if fs::read(path).ok().as_deref() == Some(json.as_slice()) {
        return Ok(json);
    }
    write_atomic_bytes(path, &json)?;
    Ok(json)
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn base_game_root(inventory_path: &Path) -> Result<PathBuf, String> {
    read_inventory(inventory_path)?
        .roots
        .into_iter()
        .find(|root| root.kind == "base-game")
        .and_then(|root| root.path)
        .ok_or_else(|| "Arma Reforger base-game add-ons folder is unavailable".to_string())
}

fn read_inventory(inventory_path: &Path) -> Result<Inventory, String> {
    let graph = read_loaded_addon_graph(inventory_path)?;
    let base_game = graph
        .addons
        .iter()
        .find(|addon| addon.guid.eq_ignore_ascii_case(BASE_GAME_GUID))
        .ok_or_else(|| "Workbench has not loaded the Arma Reforger base-game add-on".to_string())?;
    let addons_root = base_game
        .source_root
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| {
            "Workbench base-game source root has no add-ons parent directory".to_string()
        })?;
    Ok(Inventory {
        schema: "reforger-workbench-loaded-addon-graph-v1".to_string(),
        roots: vec![InventoryRoot {
            kind: "base-game".to_string(),
            path: Some(addons_root),
        }],
        addons: Vec::new(),
    })
}

fn read_loaded_addon_graph(inventory_path: &Path) -> Result<LoadedAddonGraph, String> {
    let raw = fs::read_to_string(inventory_path).map_err(|error| {
        format!(
            "Failed to read add-on source inventory {}: {error}",
            inventory_path.display()
        )
    })?;
    let graph: WorkbenchLoadedAddonGraphInventory =
        serde_json::from_str(&raw).map_err(|error| {
            format!(
                "Invalid Workbench loaded add-on graph {}: {error}",
                inventory_path.display()
            )
        })?;
    if graph.schema != "reforger-workbench-loaded-addon-graph-v1" || graph.protocol_version != 1 {
        return Err("Unsupported Workbench loaded add-on graph schema or protocol".to_string());
    }
    if graph.bridge_version.is_empty() || graph.addons.is_empty() {
        return Err("Workbench loaded add-on graph is empty or malformed".to_string());
    }
    let mut loaded_instances = BTreeSet::new();
    let addons = graph
        .addons
        .into_iter()
        .map(|addon| {
            let guid = addon.guid.to_ascii_uppercase();
            let source_root = fs::canonicalize(&addon.source_root).map_err(|_| {
                "Workbench loaded add-on graph contains an inaccessible source root".to_string()
            })?;
            if guid.len() != 16
                || !guid.bytes().all(|byte| byte.is_ascii_hexdigit())
                || addon.id.is_empty()
                || addon.title.is_empty()
                || !loaded_instances.insert((guid.clone(), source_root.clone()))
            {
                return Err(
                    "Workbench loaded add-on graph contains an invalid or duplicate GUID/source-root instance"
                        .to_string(),
                );
            }
            Ok(LoadedAddonSource {
                guid,
                id: addon.id,
                title: addon.title,
                source_root,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(LoadedAddonGraph { addons })
}

pub fn read_loaded_addon_sources(
    inventory_path: &Path,
) -> Result<Vec<LoadedAddonSourceInfo>, String> {
    Ok(read_loaded_addon_graph(inventory_path)?
        .addons
        .into_iter()
        .map(|addon| LoadedAddonSourceInfo {
            guid: addon.guid,
            display_id: addon.id,
            title: addon.title,
            source_root: addon.source_root,
        })
        .collect())
}

/// Reads the last Workbench graph even when one of its source roots is no
/// longer present. Resource metadata may still be served from its exact
/// per-instance cache; callers must label that provenance as stale.
pub fn read_loaded_addon_sources_allow_stale(
    inventory_path: &Path,
) -> Result<Vec<LoadedAddonSourceInfo>, String> {
    let raw = fs::read_to_string(inventory_path).map_err(|error| {
        format!(
            "Failed to read add-on source inventory {}: {error}",
            inventory_path.display()
        )
    })?;
    let graph: WorkbenchLoadedAddonGraphInventory =
        serde_json::from_str(&raw).map_err(|error| {
            format!(
                "Invalid Workbench loaded add-on graph {}: {error}",
                inventory_path.display()
            )
        })?;
    if graph.schema != "reforger-workbench-loaded-addon-graph-v1"
        || graph.protocol_version != 1
        || graph.bridge_version.is_empty()
        || graph.addons.is_empty()
    {
        return Err("Unsupported Workbench loaded add-on graph schema or protocol".to_string());
    }
    let mut instances = BTreeSet::new();
    graph
        .addons
        .into_iter()
        .map(|addon| {
            let guid = addon.guid.to_ascii_uppercase();
            if guid.len() != 16
                || !guid.bytes().all(|byte| byte.is_ascii_hexdigit())
                || addon.id.is_empty()
                || addon.title.is_empty()
                || !addon.source_root.is_absolute()
                || !instances.insert((guid.clone(), addon.source_root.clone()))
            {
                return Err(
                    "Workbench loaded add-on graph contains an invalid or duplicate instance"
                        .to_string(),
                );
            }
            Ok(LoadedAddonSourceInfo {
                guid,
                display_id: addon.id,
                title: addon.title,
                source_root: addon.source_root,
            })
        })
        .collect()
}

fn modified_unix_ms(metadata: &fs::Metadata) -> u128 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|time| time.as_millis())
        .unwrap_or(0)
}

fn normalized_relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn selected_payload_digest(
    archive_path: &Path,
    entries: &[PakEntry],
    control: &IndexBuildControl,
) -> Result<(String, BTreeMap<String, String>), String> {
    let mut entries = entries.iter().collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.offset());
    let mut file = fs::File::open(archive_path).map_err(|error| error.to_string())?;
    let mut hasher = Sha256::new();
    let mut entry_digests = BTreeMap::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    for entry in entries {
        control.check()?;
        hasher.update(entry.logical_path().as_bytes());
        hasher.update(entry.offset().to_le_bytes());
        hasher.update(entry.compressed_length().to_le_bytes());
        hasher.update(entry.original_length().to_le_bytes());
        hasher.update(entry.compression().to_le_bytes());
        let mut entry_hasher = Sha256::new();
        file.seek(SeekFrom::Start(entry.offset()))
            .map_err(|error| error.to_string())?;
        let mut remaining = entry.compressed_length();
        while remaining > 0 {
            control.check()?;
            let count = usize::try_from(remaining.min(buffer.len() as u64)).unwrap();
            file.read_exact(&mut buffer[..count])
                .map_err(|error| error.to_string())?;
            hasher.update(&buffer[..count]);
            entry_hasher.update(&buffer[..count]);
            remaining -= count as u64;
        }
        entry_digests.insert(
            entry.logical_path().to_string(),
            format!("{:x}", entry_hasher.finalize()),
        );
    }
    Ok((format!("{:x}", hasher.finalize()), entry_digests))
}

fn packed_source_revision(inspection: &BaseGameInspection) -> Arc<PackedSourceRevision> {
    let script_digests = inspection
        .scripts
        .iter()
        .map(|locator| {
            (
                (
                    locator.pack_relative_path.clone(),
                    locator.logical_path.clone(),
                ),
                locator.compressed_payload_sha256.clone(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut entries = Vec::new();
    for (_, selected) in &inspection.archives {
        for entry in selected {
            let pack_relative_path = normalized_relative(&inspection.root, entry.archive_path());
            let compressed_payload_sha256 = script_digests
                .get(&(pack_relative_path, entry.logical_path().to_string()))
                .cloned()
                .unwrap_or_default();
            entries.push(PackedSourceEntry {
                entry: entry.clone(),
                compressed_payload_sha256,
            });
        }
    }
    entries.sort_by(|left, right| left.entry.logical_path().cmp(right.entry.logical_path()));
    let artifacts = inspection
        .archives
        .iter()
        .map(|(archive, _)| {
            let metadata = fs::metadata(archive.path()).expect("inspected archive still exists");
            ArtifactStamp {
                path: archive.path().to_path_buf(),
                bytes: metadata.len(),
                modified_unix_ms: modified_unix_ms(&metadata),
            }
        })
        .collect();
    Arc::new(PackedSourceRevision { artifacts, entries })
}

fn revision_key(guid: &str, revision: &str) -> String {
    format!(
        "{}/{}",
        guid.to_ascii_uppercase(),
        revision.to_ascii_lowercase()
    )
}

fn addon_instance_key(guid: &str, source_root: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(guid.to_ascii_uppercase().as_bytes());
    hasher.update(b"\0");
    hasher.update(
        source_root
            .to_string_lossy()
            .to_ascii_lowercase()
            .as_bytes(),
    );
    format!("{}-{:x}", guid.to_ascii_uppercase(), hasher.finalize())
}

fn register_source_revision(guid: &str, revision: &str, sources: Arc<PackedSourceRevision>) {
    if let Ok(mut revisions) = SOURCE_REVISIONS.get_or_init(Default::default).lock() {
        revisions.insert(revision_key(guid, revision), sources);
    }
}

fn register_cached_source_revision_root(guid: &str, revision: &str, cache_root: &Path) {
    if let Ok(mut roots) = SOURCE_REVISION_ROOTS.get_or_init(Default::default).lock() {
        roots.insert(revision_key(guid, revision), cache_root.to_path_buf());
    }
}

fn load_cached_source_revision(key: &str) -> Result<(), String> {
    let cache_root = SOURCE_REVISION_ROOTS
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| "Packed source revision roots are unavailable".to_string())?
        .get(key)
        .cloned()
        .ok_or_else(|| "Packed source revision is not registered".to_string())?;
    let header_path = cache_root.join(ADDON_MANIFEST_HEADER_FILE);
    let header = match fs::read(&header_path) {
        Ok(bytes) => serde_json::from_slice::<AddonIndexManifestHeader>(&bytes)
            .map_err(|error| format!("Invalid cached add-on manifest header: {error}"))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let manifest = serde_json::from_slice::<AddonIndexManifest>(
                &fs::read(cache_root.join("manifest.json")).map_err(|read_error| {
                    format!("Failed to read cached add-on manifest: {read_error}")
                })?,
            )
            .map_err(|error| format!("Invalid cached add-on manifest: {error}"))?;
            manifest.header()
        }
        Err(error) => {
            return Err(format!(
                "Failed to read cached add-on manifest header: {error}"
            ))
        }
    };
    if revision_key(&header.guid, &header.revision) != key {
        return Err("Cached add-on manifest identity does not match its source URI".to_string());
    }
    match read_index_cache_locator_section(&cache_root.join("symbols.bin")) {
        Ok(Some(bytes)) => match decode_locator_table(&bytes) {
            Ok(scripts) => register_cached_source_revision_from_locators(&header, scripts),
            Err(error) => Err(format!("Invalid cached binary locator section: {error}")),
        },
        Ok(None) => register_cached_source_revision(&cache_root, &header),
        Err(error) => Err(format!("Invalid cached binary locator container: {error}")),
    }
}

fn publish_inventory_addon_manifests(
    inventory: &Inventory,
    storage_root: &Path,
    control: &IndexBuildControl,
) -> Result<(), String> {
    control.check()?;
    let revision = inventory_publication_revision(inventory, control)?;
    let publication_path = storage_root.join("inventory-current.json");
    let mut identities = BTreeMap::<String, PathBuf>::new();
    let mut manifests = Vec::new();
    for addon in inventory
        .addons
        .iter()
        .filter(|addon| addon.root_kind != "base-game")
    {
        control.check()?;
        let Some(project_file) = &addon.project_file else {
            continue;
        };
        let project = fs::read_to_string(project_file)
            .map_err(|error| format!("Failed to read {}: {error}", project_file.display()))?;
        let guid = extract_project_property(&project, "GUID")
            .ok_or_else(|| format!("Add-on project has no GUID: {}", project_file.display()))?
            .to_ascii_uppercase();
        if guid.len() != 16 || !guid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(format!(
                "Invalid add-on GUID '{guid}': {}",
                project_file.display()
            ));
        }
        if let Some(existing) = identities.insert(guid.clone(), addon.path.clone()) {
            return Err(format!(
                "Duplicate add-on GUID {guid}: {} and {}",
                existing.display(),
                addon.path.display()
            ));
        }
        let display_id = extract_project_property(&project, "ID")
            .or_else(|| extract_project_property(&project, "TITLE"))
            .unwrap_or_else(|| addon.directory_name.clone());
        let pack_artifacts = addon
            .pack_files
            .iter()
            .map(|path| {
                let metadata = fs::metadata(path)
                    .map_err(|error| format!("Failed to stat {}: {error}", path.display()))?;
                Ok(InventoryPackArtifact {
                    path: path.clone(),
                    bytes: metadata.len(),
                    modified_unix_ms: modified_unix_ms(&metadata),
                    strong_manifest_sha512: adjacent_manifest_sha512(path),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        manifests.push((
            storage_root.join(&guid).join("inventory.json"),
            InventoryAddonManifest {
                schema: "reforger-addon-inventory-manifest-v1",
                guid,
                display_id,
                directory_name: addon.directory_name.clone(),
                root_kind: addon.root_kind.clone(),
                source_root: addon.path.clone(),
                semantic_status: "inventory-only",
                pack_artifacts,
            },
        ));
    }
    let publication_reusable = fs::read(&publication_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<InventoryPublication>(&bytes).ok())
        .is_some_and(|publication| {
            publication.schema == "reforger-addon-inventory-publication-v1"
                && publication.revision == revision
        })
        && manifests.iter().all(|(path, expected)| {
            serde_json::to_vec_pretty(expected)
                .ok()
                .is_some_and(|mut expected_bytes| {
                    expected_bytes.push(b'\n');
                    fs::read(path)
                        .ok()
                        .is_some_and(|actual_bytes| actual_bytes == expected_bytes)
                })
        });
    if publication_reusable {
        return Ok(());
    }
    let workers = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .min(8)
        .min(manifests.len().max(1));
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        let manifests = &manifests;
        for worker in 0..workers {
            handles.push(scope.spawn(move || {
                for (path, manifest) in manifests.iter().skip(worker).step_by(workers) {
                    control.check()?;
                    write_json_atomic(path, manifest)?;
                }
                Ok::<(), String>(())
            }));
        }
        for handle in handles {
            handle
                .join()
                .map_err(|_| "Add-on inventory publication worker panicked".to_string())??;
        }
        Ok::<(), String>(())
    })?;
    write_json_atomic(
        &publication_path,
        &InventoryPublication {
            schema: "reforger-addon-inventory-publication-v1".to_string(),
            revision,
        },
    )?;
    Ok(())
}

fn inventory_publication_revision(
    inventory: &Inventory,
    control: &IndexBuildControl,
) -> Result<String, String> {
    let mut hasher = Sha256::new();
    hasher.update(b"reforger-addon-inventory-publication-v2");
    for addon in inventory
        .addons
        .iter()
        .filter(|addon| addon.root_kind != "base-game")
    {
        control.check()?;
        hasher.update(addon.root_kind.as_bytes());
        hasher.update(addon.directory_name.as_bytes());
        hasher.update(addon.path.to_string_lossy().as_bytes());
        if let Some(project_file) = &addon.project_file {
            hasher.update(project_file.to_string_lossy().as_bytes());
            hasher.update(
                fs::read(project_file).map_err(|error| {
                    format!("Failed to read {}: {error}", project_file.display())
                })?,
            );
        }
        for path in &addon.pack_files {
            control.check()?;
            let metadata = fs::metadata(path)
                .map_err(|error| format!("Failed to stat {}: {error}", path.display()))?;
            hasher.update(path.to_string_lossy().as_bytes());
            hasher.update(metadata.len().to_le_bytes());
            if let Some(strong_manifest) = adjacent_manifest_sha512(path) {
                hasher.update(b"reforger-manifest-sha512");
                hasher.update(strong_manifest.as_bytes());
                continue;
            }
            let archive = PakArchive::inspect_with_cancel(path, || control.is_cancelled())
                .map_err(|error| format!("Failed to inspect {}: {error}", path.display()))?;
            let entries = archive.select(PakSelection::scripts()).map_err(|error| {
                format!("Failed to select scripts from {}: {error}", path.display())
            })?;
            let (selected_digest, _) = selected_payload_digest(path, &entries, control)?;
            hasher.update(selected_digest.as_bytes());
        }
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Reads only the dependency field from the two observed `.gproj` forms:
/// `Dependencies { "GUID" }` and `Dependencies GUID`. This fallback parser
/// deliberately ignores every other project property; Workbench remains the
/// authority for the effective loaded graph.
struct DependencyProjectCandidate {
    addon: LoadedAddonSource,
    project_file: PathBuf,
}

fn read_project_dependency_graph(
    project_files: &[PathBuf],
    workbench_profile: Option<&Path>,
    control: &IndexBuildControl,
) -> Result<LoadedAddonGraph, String> {
    if project_files.is_empty() {
        return Err("No opened project descriptor was provided".to_string());
    }

    let mut discovered = BTreeSet::new();
    for project_file in project_files {
        discovered.insert(fs::canonicalize(project_file).map_err(|error| {
            format!(
                "Failed to resolve dependency project {}: {error}",
                project_file.display()
            )
        })?);
    }
    if let Some(profile) = workbench_profile {
        for project_file in
            registered_project_files(crate::host_platform::workbench_host(), profile)?
        {
            if project_file.is_file() {
                discovered.insert(fs::canonicalize(&project_file).map_err(|error| {
                    format!(
                        "Failed to resolve Workbench project-list entry {}: {error}",
                        project_file.display()
                    )
                })?);
            }
        }
    }
    for project_file in installed_game_addon_project_files()? {
        discovered.insert(project_file);
    }

    let mut candidates = BTreeMap::<String, Vec<DependencyProjectCandidate>>::new();
    for project_file in discovered {
        control.check()?;
        if let Ok(candidate) = read_dependency_project_candidate(&project_file) {
            candidates
                .entry(candidate.addon.guid.clone())
                .or_default()
                .push(candidate);
        }
    }
    for candidates in candidates.values_mut() {
        candidates.sort_by(|left, right| {
            dependency_source_preference(&left.addon.source_root)
                .cmp(&dependency_source_preference(&right.addon.source_root))
                .then_with(|| left.project_file.cmp(&right.project_file))
        });
    }

    let mut queue = project_files
        .iter()
        .map(|project_file| fs::canonicalize(project_file).map_err(|error| error.to_string()))
        .collect::<Result<Vec<_>, String>>()?;
    if let Some(base_game) = candidates
        .get(BASE_GAME_GUID)
        .and_then(|candidates| candidates.first())
    {
        queue.push(base_game.project_file.clone());
    }

    let mut addons = BTreeMap::<String, LoadedAddonSource>::new();
    let mut visited_projects = BTreeSet::new();
    while let Some(project_file) = queue.pop() {
        control.check()?;
        if !visited_projects.insert(project_file.clone()) {
            continue;
        }
        let candidate = read_dependency_project_candidate(&project_file)?;
        addons
            .entry(candidate.addon.guid.clone())
            .or_insert_with(|| candidate.addon.clone());
        for dependency_guid in read_project_dependency_guids(&[project_file])? {
            let Some(dependency) = candidates
                .get(&dependency_guid)
                .and_then(|candidates| candidates.first())
            else {
                continue;
            };
            addons
                .entry(dependency.addon.guid.clone())
                .or_insert_with(|| dependency.addon.clone());
            queue.push(dependency.project_file.clone());
        }
    }

    Ok(LoadedAddonGraph {
        addons: addons.into_values().collect(),
    })
}

/// Collects the GUID closure without opening archives. This powers the
/// optimistic cache hydration pass; the subsequent build pass performs the
/// stronger source-usability and ambiguity checks.
fn read_project_dependency_scope_guids(
    project_files: &[PathBuf],
    storage_root: &Path,
    control: &IndexBuildControl,
) -> Result<BTreeSet<String>, String> {
    if read_cache_catalogue(storage_root).is_some() {
        let _ = cached_manifest_descriptors(storage_root, control)?;
        if read_cache_catalogue(storage_root).is_none() {
            let cached_project_files = cached_dependency_project_files(storage_root)?;
            return read_project_dependency_scope_guids_from_candidates(
                project_files,
                &cached_project_files,
            );
        }
        return read_catalogued_project_dependency_scope_guids(
            project_files,
            storage_root,
            control,
        );
    }
    let cached_project_files = cached_dependency_project_files(storage_root)?;
    read_project_dependency_scope_guids_from_candidates(project_files, &cached_project_files)
}

fn read_catalogued_project_dependency_scope_guids(
    project_files: &[PathBuf],
    storage_root: &Path,
    control: &IndexBuildControl,
) -> Result<BTreeSet<String>, String> {
    if project_files.is_empty() {
        return Err("No opened project descriptor was provided".to_string());
    }
    let catalogue = read_cache_catalogue(storage_root)
        .ok_or_else(|| "Add-on cache catalogue is unavailable".to_string())?;
    let mut scope = BTreeSet::from([BASE_GAME_GUID.to_string()]);
    let mut queue = project_files
        .iter()
        .map(|project_file| fs::canonicalize(project_file).map_err(|error| error.to_string()))
        .collect::<Result<Vec<_>, String>>()?;
    let mut visited_projects = BTreeSet::new();
    while let Some(project_file) = queue.pop() {
        control.check()?;
        if !visited_projects.insert(project_file.clone()) {
            continue;
        }
        if let Ok(candidate) = read_dependency_project_candidate(&project_file) {
            scope.insert(candidate.addon.guid);
        }
        for dependency_guid in read_project_dependency_guids(&[project_file])? {
            scope.insert(dependency_guid.clone());
            for manifest in catalogue
                .entries
                .iter()
                .filter(|manifest| manifest.guid.eq_ignore_ascii_case(&dependency_guid))
            {
                if manifest.source_root.is_dir() {
                    let mut dependency_projects = BTreeSet::new();
                    if collect_gproj_files(&manifest.source_root, &mut dependency_projects).is_ok()
                    {
                        queue.extend(dependency_projects);
                    }
                }
            }
        }
    }
    Ok(scope)
}

fn read_project_dependency_scope_guids_from_candidates(
    project_files: &[PathBuf],
    candidate_project_files: &[PathBuf],
) -> Result<BTreeSet<String>, String> {
    if project_files.is_empty() {
        return Err("No opened project descriptor was provided".to_string());
    }
    let mut discovery_inputs = project_files.to_vec();
    discovery_inputs.extend(candidate_project_files.iter().cloned());
    let candidate_files = discover_dependency_project_files(&discovery_inputs)?;
    let mut candidates = BTreeMap::<String, Vec<PathBuf>>::new();
    for project_file in candidate_files {
        if let Ok(candidate) = read_dependency_project_candidate(&project_file) {
            candidates
                .entry(candidate.addon.guid)
                .or_default()
                .push(candidate.project_file);
        }
    }
    let mut scope = BTreeSet::from([BASE_GAME_GUID.to_string()]);
    let mut queue = Vec::new();
    for project_file in project_files {
        let project_file = fs::canonicalize(project_file).map_err(|error| error.to_string())?;
        if let Ok(candidate) = read_dependency_project_candidate(&project_file) {
            scope.insert(candidate.addon.guid.clone());
            queue.push(candidate.project_file);
        } else {
            queue.push(project_file);
        }
    }
    let mut visited_projects = BTreeSet::new();
    while let Some(project_file) = queue.pop() {
        if !visited_projects.insert(project_file.clone()) {
            continue;
        }
        for dependency_guid in read_project_dependency_guids(&[project_file])? {
            scope.insert(dependency_guid.clone());
            if let Some(dependency_projects) = candidates.get(&dependency_guid) {
                queue.extend(dependency_projects.iter().cloned());
            }
        }
    }
    Ok(scope)
}

fn cached_dependency_project_files(storage_root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut project_files = BTreeSet::new();
    for (manifest, _) in cached_manifest_descriptors(storage_root, &IndexBuildControl::default())? {
        if manifest.schema != ADDON_MANIFEST_SCHEMA {
            continue;
        }
        if let Ok(entries) = fs::read_dir(&manifest.source_root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file()
                    && path
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("gproj"))
                {
                    if let Ok(path) = fs::canonicalize(path) {
                        project_files.insert(path);
                    }
                }
            }
        }
    }

    Ok(project_files.into_iter().collect())
}

fn discover_dependency_project_files(project_files: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let mut discovered = BTreeSet::new();
    for project_file in project_files {
        let project_file = fs::canonicalize(project_file).map_err(|error| {
            format!(
                "Failed to resolve dependency project {}: {error}",
                project_file.display()
            )
        })?;
        discovered.insert(project_file.clone());
        let Some(parent) = project_file.parent() else {
            continue;
        };
        collect_gproj_files(parent, &mut discovered)?;
        if let Some(addons_directory) = parent.parent().filter(|directory| {
            directory
                .file_name()
                .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("addons"))
        }) {
            for entry in fs::read_dir(addons_directory)
                .map_err(|error| format!("Failed to read {}: {error}", addons_directory.display()))?
                .flatten()
            {
                if entry.path().is_dir() {
                    collect_gproj_files(&entry.path(), &mut discovered)?;
                }
            }
        }
    }
    if let Some(profile) = crate::workbench::default_profile_directory() {
        if let Ok(projects) =
            registered_project_files(crate::host_platform::workbench_host(), &profile)
        {
            for project in projects {
                if project.is_file() {
                    discovered.insert(project);
                }
            }
        }
    }
    Ok(discovered.into_iter().collect())
}

fn collect_gproj_files(directory: &Path, discovered: &mut BTreeSet<PathBuf>) -> Result<(), String> {
    for entry in fs::read_dir(directory)
        .map_err(|error| format!("Failed to read {}: {error}", directory.display()))?
        .flatten()
    {
        let path = entry.path();
        if path.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("gproj"))
        {
            discovered.insert(fs::canonicalize(path).map_err(|error| error.to_string())?);
        }
    }
    Ok(())
}

fn read_dependency_project_candidate(
    project_file: &Path,
) -> Result<DependencyProjectCandidate, String> {
    let source = fs::read_to_string(project_file).map_err(|error| {
        format!(
            "Failed to read dependency project {}: {error}",
            project_file.display()
        )
    })?;
    let guid = extract_project_property(&source, "GUID")
        .ok_or_else(|| format!("Dependency project has no GUID: {}", project_file.display()))?
        .to_ascii_uppercase();
    if guid.len() != 16 || !guid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!(
            "Invalid dependency project GUID: {}",
            project_file.display()
        ));
    }
    let source_root = project_file.parent().ok_or_else(|| {
        format!(
            "Dependency project has no source root: {}",
            project_file.display()
        )
    })?;
    let source_root = fs::canonicalize(source_root).map_err(|error| error.to_string())?;
    let id = extract_project_property(&source, "ID")
        .or_else(|| extract_project_property(&source, "TITLE"))
        .or_else(|| {
            source_root
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| guid.clone());
    let title = extract_project_property(&source, "TITLE").unwrap_or_else(|| id.clone());
    Ok(DependencyProjectCandidate {
        addon: LoadedAddonSource {
            guid,
            id,
            title,
            source_root,
        },
        project_file: fs::canonicalize(project_file).map_err(|error| error.to_string())?,
    })
}

fn read_project_dependency_guids(project_files: &[PathBuf]) -> Result<BTreeSet<String>, String> {
    let mut dependency_guids = BTreeSet::new();
    for project_file in project_files {
        let source = fs::read_to_string(project_file).map_err(|error| {
            format!(
                "Failed to read dependency project {}: {error}",
                project_file.display()
            )
        })?;
        let mut in_dependencies = false;
        let mut depth = 0_i32;
        for line in source.lines() {
            let trimmed = line.trim();
            if !in_dependencies {
                let Some(remainder) = trimmed.strip_prefix("Dependencies") else {
                    continue;
                };
                if !remainder
                    .chars()
                    .next()
                    .is_some_and(|value| value.is_whitespace() || value == '{')
                {
                    continue;
                }
                in_dependencies = true;
                let open_count = line.chars().filter(|value| *value == '{').count() as i32;
                if open_count == 0 {
                    add_dependency_guids(remainder, &mut dependency_guids);
                    in_dependencies = false;
                    continue;
                }
                depth = open_count - line.chars().filter(|value| *value == '}').count() as i32;
            } else {
                depth += line.chars().filter(|value| *value == '{').count() as i32;
                depth -= line.chars().filter(|value| *value == '}').count() as i32;
            }
            add_dependency_guids(line, &mut dependency_guids);
            if depth <= 0 {
                in_dependencies = false;
                depth = 0;
            }
        }
    }
    Ok(dependency_guids)
}

fn add_dependency_guids(source: &str, dependency_guids: &mut BTreeSet<String>) {
    let source = source.split_once("//").map_or(source, |(code, _)| code);
    let mut candidate = String::new();
    let flush = |candidate: &mut String, dependency_guids: &mut BTreeSet<String>| {
        if candidate.len() == 16 && candidate.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            dependency_guids.insert(candidate.to_ascii_uppercase());
        }
        candidate.clear();
    };
    for character in source.chars() {
        if character.is_ascii_hexdigit() {
            candidate.push(character);
        } else {
            flush(&mut candidate, dependency_guids);
        }
    }
    flush(&mut candidate, dependency_guids);
}

fn extract_project_property(source: &str, property: &str) -> Option<String> {
    source.lines().find_map(|line| {
        let remainder = line.trim().strip_prefix(property)?;
        if !remainder
            .chars()
            .next()
            .is_some_and(|value| value.is_whitespace() || value == ':')
        {
            return None;
        }
        let remainder = remainder
            .trim_start()
            .strip_prefix(':')
            .unwrap_or(remainder)
            .trim_start();
        let quoted = remainder.strip_prefix('"')?;
        let end = quoted.find('"')?;
        (!quoted[..end].is_empty()).then(|| quoted[..end].to_string())
    })
}

fn adjacent_manifest_sha512(pack: &Path) -> Option<String> {
    let file_name = pack.file_name()?.to_string_lossy();
    let prefix = format!("{file_name}_");
    let parent = pack.parent()?;
    let mut candidates = fs::read_dir(parent)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .map(|name| {
                    let name = name.to_string_lossy();
                    name.starts_with(&prefix) && name.ends_with("_manifest.json")
                })
                .unwrap_or(false)
        })
        .collect::<Vec<_>>();
    candidates.sort();
    let raw = fs::read_to_string(candidates.first()?).ok()?;
    serde_json::from_str::<serde_json::Value>(&raw)
        .ok()?
        .get("sha512")?
        .as_str()
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index_cache::IndexCacheStatus;

    #[test]
    fn valid_packed_source_reuses_its_decoded_buffer() {
        let bytes = b"class SCR_Example {}".to_vec();
        let buffer = bytes.as_ptr();

        let source = decode_source_bytes(bytes);

        assert_eq!(source, "class SCR_Example {}");
        assert_eq!(source.as_ptr(), buffer);
    }

    #[test]
    fn invalid_packed_source_preserves_lossy_decoding() {
        let bytes = vec![b'a', 0xff, b'b'];
        let expected = String::from_utf8_lossy(&bytes).into_owned();

        assert_eq!(decode_source_bytes(bytes), expected);
    }

    #[test]
    fn rejects_unknown_inventory_schema() {
        let root =
            std::env::temp_dir().join(format!("reforger_inventory_test_{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let inventory = root.join("inventory.json");
        fs::write(&inventory, r#"{"schema":"unknown","roots":[]}"#).unwrap();
        assert!(build_base_game_index(&inventory, &IndexBuildControl::default()).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn discovers_loose_scripts_only_from_the_addon_scripts_directory() {
        let root = test_root("loose_script_paths");
        let scripts = root.join("Scripts").join("Game");
        let assets = root.join("Assets");
        fs::create_dir_all(&scripts).unwrap();
        fs::create_dir_all(&assets).unwrap();
        fs::write(scripts.join("Listed.c"), "class Listed {}").unwrap();
        fs::write(assets.join("Ignored.c"), "class Ignored {}").unwrap();
        fs::write(root.join("Ignored.c"), "class Ignored {}").unwrap();

        assert_eq!(
            loose_script_paths(&root).unwrap(),
            vec![scripts.join("Listed.c")]
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn reads_dependency_guids_from_project_files() {
        let root = test_root("project_dependencies");
        let project = root.join("addon.gproj");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            &project,
            "GameProject {\n Dependencies {\n  \"58D0FB3206B6F859\"\n  \"6954AAD9FD5A27CC\"\n }\n}",
        )
        .unwrap();

        assert_eq!(
            read_project_dependency_guids(&[project]).unwrap(),
            BTreeSet::from([
                "58D0FB3206B6F859".to_string(),
                "6954AAD9FD5A27CC".to_string(),
            ])
        );
        let flat_project = root.join("flat.gproj");
        fs::write(
            &flat_project,
            "GameProject {\n Dependencies 58D0FB3206B6F859\n}",
        )
        .unwrap();
        assert_eq!(
            read_project_dependency_guids(&[flat_project]).unwrap(),
            BTreeSet::from(["58D0FB3206B6F859".to_string()])
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn dependency_cache_prefers_unpacked_source_for_a_duplicate_guid() {
        let root = test_root("dependency_cache_preference");
        let packed = root.join("packed");
        let unpacked = root.join("unpacked");
        fs::create_dir_all(&packed).unwrap();
        fs::create_dir_all(unpacked.join("Scripts")).unwrap();
        write_fixture_pak(
            &packed.join("data.pak"),
            &[("Packed.c", b"class Packed {}")],
        );
        write_fixture_pak(
            &unpacked.join("data.pak"),
            &[("UnpackedPacked.c", b"class UnpackedPacked {}")],
        );
        fs::write(unpacked.join("Scripts/Unpacked.c"), "class Unpacked {}\n").unwrap();
        let graph = root.join("graph.json");
        fs::write(
            &graph,
            format!(
                r#"{{"schema":"reforger-workbench-loaded-addon-graph-v1","bridgeVersion":"1.52.0","protocolVersion":1,"addons":[{{"guid":"1111111111111111","id":"Same","title":"Packed","sourceRoot":{}}},{{"guid":"1111111111111111","id":"Same","title":"Unpacked","sourceRoot":{}}}]}}"#,
                serde_json::to_string(&packed).unwrap(),
                serde_json::to_string(&unpacked).unwrap(),
            ),
        )
        .unwrap();
        let storage = root.join("indexes");
        load_or_build_loaded_addon_indexes(&graph, &storage, &[], &IndexBuildControl::default())
            .unwrap();
        let project = root.join("project.gproj");
        fs::write(
            &project,
            "GameProject {\n Dependencies {\n  \"1111111111111111\"\n }\n}",
        )
        .unwrap();

        let selected = load_cached_dependency_addon_indexes(
            std::slice::from_ref(&project),
            &storage,
            &[],
            &IndexBuildControl::default(),
        )
        .unwrap();
        assert_eq!(selected.loaded_instances, 1);
        assert_eq!(selected.summary.files, 2);
        let resource_sources = read_cached_dependency_addon_sources(
            &[project],
            &storage,
            &IndexBuildControl::default(),
        )
        .unwrap();
        assert_eq!(resource_sources.len(), 1);
        assert_eq!(
            resource_sources[0].source_root,
            fs::canonicalize(unpacked).unwrap()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn offline_dependency_startup_builds_missing_cache_from_workbench_project_list() {
        let root = test_root("offline_dependency_build_on_miss");
        let profile = root.join("profile");
        let workspace = root.join("workspace");
        let packed = root.join("packed");
        let unpacked = root.join("unpacked");
        let transitive = root.join("transitive");
        fs::create_dir_all(&profile).unwrap();
        fs::create_dir_all(workspace.join("Scripts")).unwrap();
        fs::create_dir_all(packed.join("Scripts")).unwrap();
        fs::create_dir_all(unpacked.join("Scripts")).unwrap();
        fs::create_dir_all(transitive.join("Scripts")).unwrap();
        fs::write(
            workspace.join("project.gproj"),
            "GameProject {\n GUID \"AAAAAAAAAAAAAAAA\"\n Dependencies { \"1111111111111111\" }\n }",
        )
        .unwrap();
        fs::write(
            workspace.join("Scripts/Workspace.c"),
            "class Workspace {}\n",
        )
        .unwrap();
        fs::write(
            packed.join("addon.gproj"),
            "GameProject {\n GUID \"1111111111111111\"\n Dependencies { \"2222222222222222\" }\n }\n",
        )
        .unwrap();
        fs::write(
            unpacked.join("addon.gproj"),
            "GameProject {\n GUID \"1111111111111111\"\n Dependencies { \"2222222222222222\" }\n }\n",
        )
        .unwrap();
        write_fixture_pak(
            &packed.join("data.pak"),
            &[("Packed.c", b"class Packed {}")],
        );
        fs::write(unpacked.join("Scripts/Unpacked.c"), "class Unpacked {}\n").unwrap();
        fs::write(
            transitive.join("addon.gproj"),
            "GameProject {\n GUID \"2222222222222222\"\n }\n",
        )
        .unwrap();
        fs::write(
            transitive.join("Scripts/Transitive.c"),
            "class Transitive {}\n",
        )
        .unwrap();
        fs::write(
            profile.join(".projectList_app1874910_user.conf"),
            format!(
                "FilePath \"{}\"\nFilePath \"{}\"\nFilePath \"{}\"\nFilePath \"{}\"\n",
                workspace.join("project.gproj").display(),
                packed.join("addon.gproj").display(),
                unpacked.join("addon.gproj").display(),
                transitive.join("addon.gproj").display(),
            ),
        )
        .unwrap();

        let storage = root.join("indexes");
        let first = load_or_build_dependency_addon_indexes(
            &[workspace.join("project.gproj")],
            Some(&profile),
            &storage,
            &[workspace.join("Scripts")],
            &IndexBuildControl::default(),
        )
        .unwrap();

        assert_eq!(first.rebuilt_instances, 2);
        assert_eq!(first.loaded_instances, 0);
        assert_eq!(first.summary.files, 2);
        assert!(first.index.top_level_symbols_for_name("Packed").is_empty());
        assert!(!first
            .index
            .top_level_symbols_for_name("Unpacked")
            .is_empty());
        assert!(!first
            .index
            .top_level_symbols_for_name("Transitive")
            .is_empty());

        let second = load_or_build_dependency_addon_indexes(
            &[workspace.join("project.gproj")],
            Some(&profile),
            &storage,
            &[workspace.join("Scripts")],
            &IndexBuildControl::default(),
        )
        .unwrap();
        assert_eq!(second.rebuilt_instances, 0);
        assert_eq!(second.loaded_instances, 2);
        assert_eq!(second.summary.files, 2);

        let packed_cache = fs::read_dir(&storage)
            .unwrap()
            .flatten()
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("1111111111111111-")
            })
            .unwrap()
            .path()
            .join("symbols.bin");
        fs::remove_file(packed_cache).unwrap();
        let repaired = load_or_build_dependency_addon_indexes(
            &[workspace.join("project.gproj")],
            Some(&profile),
            &storage,
            &[workspace.join("Scripts")],
            &IndexBuildControl::default(),
        )
        .unwrap();
        assert_eq!(repaired.rebuilt_instances, 1);
        assert_eq!(repaired.loaded_instances, 1);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn offline_dependency_scope_includes_the_cached_base_game_index() {
        let root = test_root("dependency_scope_cached_base_game");
        let addons = root.join("addons");
        let base_game_root = addons.join("core");
        let project_root = addons.join("project");
        fs::create_dir_all(&base_game_root).unwrap();
        fs::create_dir_all(project_root.join("Scripts")).unwrap();
        write_fixture_pak(
            &base_game_root.join("data.pak"),
            &[("BaseGame.c", b"class BaseGame {}")],
        );
        fs::write(
            project_root.join("project.gproj"),
            format!(
                "GameProject {{\n ID \"Project\"\n GUID \"AAAAAAAAAAAAAAAA\"\n Dependencies {{}}\n}}"
            ),
        )
        .unwrap();
        fs::write(project_root.join("Scripts/Project.c"), "class Project {}\n").unwrap();

        let storage = root.join("indexes");
        let base_game = LoadedAddonSource {
            guid: BASE_GAME_GUID.to_string(),
            id: "ArmaReforger".to_string(),
            title: "Arma Reforger".to_string(),
            source_root: fs::canonicalize(&base_game_root).unwrap(),
        };
        let base_result = load_or_build_addon_indexes(
            LoadedAddonGraph {
                addons: vec![base_game],
            },
            Duration::ZERO,
            &storage,
            &[],
            &IndexBuildControl::default(),
            AddonScopeAuthority::WorkbenchLoaded,
        )
        .unwrap();
        assert_eq!(base_result.rebuilt_instances, 1);
        let manifest: AddonIndexManifest = serde_json::from_slice(
            &fs::read(storage.join(format!(
                "{}-{}/manifest.json",
                BASE_GAME_GUID,
                addon_instance_key(
                    BASE_GAME_GUID,
                    &fs::canonicalize(&base_game_root).unwrap(),
                )
                .split_once('-')
                .map(|(_, digest)| digest)
                .unwrap(),
            )))
            .unwrap(),
        )
        .unwrap();
        let virtual_source = manifest.scripts.first().unwrap().uri.clone();
        SOURCE_REVISIONS
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .remove(&revision_key(&manifest.guid, &manifest.revision));

        let result = load_cached_dependency_addon_indexes(
            &[project_root.join("project.gproj")],
            &storage,
            &[project_root.join("Scripts")],
            &IndexBuildControl::default(),
        )
        .unwrap();

        assert_eq!(
            result.scope_authority,
            AddonScopeAuthority::ProjectDependencies
        );
        assert_eq!(
            result.loaded_instances, 1,
            "cached base game should be loaded"
        );
        assert_eq!(
            result.instances.len(),
            1,
            "the workspace project is not game data"
        );
        assert_eq!(result.instances[0].guid, BASE_GAME_GUID);
        assert_eq!(result.summary.files, 1);
        fs::remove_file(storage.join(format!(
                "{}-{}/manifest.json",
                BASE_GAME_GUID,
                addon_instance_key(
                    BASE_GAME_GUID,
                    &fs::canonicalize(&base_game_root).unwrap(),
                )
                .split_once('-')
                .map(|(_, digest)| digest)
                .unwrap(),
            )))
        .unwrap();
        assert_eq!(
            read_virtual_source(&virtual_source).unwrap(),
            "class BaseGame {}"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn offline_dependency_scope_includes_the_cached_core_addon() {
        let root = test_root("dependency_scope_cached_core");
        let installed_addons = root.join("steam").join("addons");
        let user_addons = root.join("user").join("addons");
        let base_game_root = installed_addons.join("data");
        let core_root = installed_addons.join("core");
        let project_root = user_addons.join("project");
        fs::create_dir_all(&base_game_root).unwrap();
        fs::create_dir_all(&core_root).unwrap();
        fs::create_dir_all(project_root.join("Scripts")).unwrap();
        write_fixture_pak(
            &base_game_root.join("data.pak"),
            &[("BaseGame.c", b"class BaseGame {}")],
        );
        write_fixture_pak(
            &core_root.join("data.pak"),
            &[
                ("RplRpc.c", b"class RplRpc : UniqueAttribute {}"),
                ("RplChannel.c", b"enum RplChannel { Reliable }"),
                ("RplRcver.c", b"enum RplRcver { Server }"),
            ],
        );
        fs::write(
            base_game_root.join("ArmaReforger.gproj"),
            "GameProject {\n ID \"ArmaReforger\"\n GUID \"58D0FB3206B6F859\"\n Dependencies {\n  \"5614BBCCBB55ED1C\"\n }\n }",
        )
        .unwrap();
        fs::write(
            project_root.join("project.gproj"),
            "GameProject {\n ID \"Project\"\n GUID \"AAAAAAAAAAAAAAAA\"\n Dependencies {\n  \"58D0FB3206B6F859\"\n }\n }",
        )
        .unwrap();

        let storage = root.join("indexes");
        load_or_build_addon_indexes(
            LoadedAddonGraph {
                addons: vec![
                    LoadedAddonSource {
                        guid: BASE_GAME_GUID.to_string(),
                        id: "ArmaReforger".to_string(),
                        title: "Arma Reforger".to_string(),
                        source_root: fs::canonicalize(&base_game_root).unwrap(),
                    },
                    LoadedAddonSource {
                        guid: "5614BBCCBB55ED1C".to_string(),
                        id: "core".to_string(),
                        title: "Enfusion core data".to_string(),
                        source_root: fs::canonicalize(&core_root).unwrap(),
                    },
                ],
            },
            Duration::ZERO,
            &storage,
            &[],
            &IndexBuildControl::default(),
            AddonScopeAuthority::WorkbenchLoaded,
        )
        .unwrap();

        let result = load_cached_dependency_addon_indexes(
            &[project_root.join("project.gproj")],
            &storage,
            &[project_root.join("Scripts")],
            &IndexBuildControl::default(),
        )
        .unwrap();

        assert_eq!(
            result.loaded_instances, 2,
            "core and base game should be loaded"
        );
        assert!(result
            .instances
            .iter()
            .any(|instance| instance.guid == "5614BBCCBB55ED1C"));
        assert!(!result.index.top_level_symbols_for_name("RplRpc").is_empty());
        assert!(!result
            .index
            .top_level_symbols_for_name("RplChannel")
            .is_empty());
        assert!(!result
            .index
            .top_level_symbols_for_name("RplRcver")
            .is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn loaded_addon_index_workers_are_bounded_by_tasks_cpus_and_the_cold_start_cap() {
        assert_eq!(addon_index_worker_count_for(1, 140), 1);
        assert_eq!(addon_index_worker_count_for(16, 0), 0);
        assert_eq!(addon_index_worker_count_for(16, 2), 2);
        assert_eq!(
            addon_index_worker_count_for(16, 140),
            MAX_ADDON_INDEX_WORKERS
        );
    }

    #[test]
    fn accepts_only_the_workbench_loaded_graph_and_derives_the_base_addons_parent() {
        let root = test_root("workbench_graph");
        let graph = root.join("graph.json");
        let data_root = root.join("addons").join("data");
        fs::create_dir_all(&data_root).unwrap();
        fs::write(data_root.join("ArmaReforger.gproj"), "{}").unwrap();
        fs::write(
            &graph,
            format!(
                r#"{{"schema":"reforger-workbench-loaded-addon-graph-v1","bridgeVersion":"1.52.0","protocolVersion":1,"addons":[{{"guid":"{BASE_GAME_GUID}","id":"ArmaReforger","title":"Arma Reforger","sourceRoot":{}}}]}}"#,
                serde_json::to_string(&data_root).unwrap(),
            ),
        )
        .unwrap();

        let inventory = read_inventory(&graph).unwrap();

        assert_eq!(inventory.roots.len(), 1);
        assert_eq!(
            inventory.roots[0].path,
            data_root
                .parent()
                .and_then(|path| fs::canonicalize(path).ok())
        );
        let legacy = root.join("legacy.json");
        fs::write(
            &legacy,
            r#"{"schema":"reforger-addon-source-inventory-v1","roots":[]}"#,
        )
        .unwrap();
        assert!(read_inventory(&legacy).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn distinguishes_loaded_instances_by_guid_and_canonical_source_root() {
        let root = test_root("duplicate_guid_loaded_instances");
        let packed = root.join("packed");
        let unpacked = root.join("unpacked");
        fs::create_dir_all(&packed).unwrap();
        fs::create_dir_all(&unpacked).unwrap();
        let graph = root.join("graph.json");
        fs::write(&graph, format!(
            r#"{{"schema":"reforger-workbench-loaded-addon-graph-v1","bridgeVersion":"1.52.0","protocolVersion":1,"addons":[{{"guid":"1111111111111111","id":"Same","title":"Packed","sourceRoot":{}}},{{"guid":"1111111111111111","id":"Same","title":"Unpacked","sourceRoot":{}}}]}}"#,
            serde_json::to_string(&packed).unwrap(),
            serde_json::to_string(&unpacked).unwrap(),
        )).unwrap();

        let loaded = read_loaded_addon_graph(&graph).unwrap();

        assert_eq!(loaded.addons.len(), 2);
        assert_ne!(
            addon_instance_key(&loaded.addons[0].guid, &loaded.addons[0].source_root),
            addon_instance_key(&loaded.addons[1].guid, &loaded.addons[1].source_root),
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unchanged_workbench_graph_matches_the_warm_instance_scope() {
        let root = test_root("warm_scope_match");
        let first_root = root.join("addons").join("first");
        let second_root = root.join("addons").join("second");
        fs::create_dir_all(&first_root).unwrap();
        fs::create_dir_all(&second_root).unwrap();
        let graph = root.join("graph.json");
        fs::write(
            &graph,
            format!(
                r#"{{"schema":"reforger-workbench-loaded-addon-graph-v1","bridgeVersion":"1.52.0","protocolVersion":1,"addons":[{{"guid":"1111111111111111","id":"First","title":"First","sourceRoot":{}}},{{"guid":"2222222222222222","id":"Second","title":"Second","sourceRoot":{}}}]}}"#,
                serde_json::to_string(&first_root).unwrap(),
                serde_json::to_string(&second_root).unwrap(),
            ),
        )
        .unwrap();

        let warm_scope = vec![
            LoadedAddonInstanceIdentity {
                guid: "1111111111111111".to_string(),
                source_root: fs::canonicalize(&first_root).unwrap(),
            },
            LoadedAddonInstanceIdentity {
                guid: "2222222222222222".to_string(),
                source_root: fs::canonicalize(&second_root).unwrap(),
            },
        ];

        assert!(loaded_workbench_graph_matches_scope(&graph, &[], &warm_scope).unwrap());
        let reversed_scope = warm_scope.iter().cloned().rev().collect::<Vec<_>>();
        assert!(!loaded_workbench_graph_matches_scope(&graph, &[], &reversed_scope).unwrap());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn one_corrupt_warm_cache_does_not_discard_other_cached_instances() {
        let root = test_root("corrupt_warm_cache");
        let first_root = root.join("addons").join("first");
        let second_root = root.join("addons").join("second");
        fs::create_dir_all(&first_root).unwrap();
        fs::create_dir_all(&second_root).unwrap();
        write_fixture_pak(
            &first_root.join("data.pak"),
            &[("First.c", b"class First {}")],
        );
        write_fixture_pak(
            &second_root.join("data.pak"),
            &[("Second.c", b"class Second {}")],
        );
        let graph = root.join("graph.json");
        fs::write(
            &graph,
            format!(
                r#"{{"schema":"reforger-workbench-loaded-addon-graph-v1","bridgeVersion":"1.52.0","protocolVersion":1,"addons":[{{"guid":"1111111111111111","id":"First","title":"First","sourceRoot":{}}},{{"guid":"2222222222222222","id":"Second","title":"Second","sourceRoot":{}}}]}}"#,
                serde_json::to_string(&first_root).unwrap(),
                serde_json::to_string(&second_root).unwrap(),
            ),
        )
        .unwrap();
        let storage = root.join("indexes");
        load_or_build_loaded_addon_indexes(&graph, &storage, &[], &IndexBuildControl::default())
            .unwrap();
        let corrupt_cache = cache_directory_for(&storage, "1111111111111111").join("symbols.bin");
        fs::write(corrupt_cache, b"corrupt").unwrap();

        let result =
            load_all_cached_addon_indexes(&storage, &[], &IndexBuildControl::default()).unwrap();
        assert_eq!(result.loaded_instances, 1);
        assert_eq!(result.missing_instances, 1);
        assert_eq!(result.unavailable_instances.len(), 1);
        assert_eq!(result.summary.files, 1);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn indexes_workspace_addons_only_through_live_sources_and_removes_their_cache() {
        let root = test_root("loaded_addon_instances");
        let packed = root.join("packed");
        let loose = root.join("loose");
        fs::create_dir_all(&packed).unwrap();
        fs::create_dir_all(loose.join("scripts")).unwrap();
        write_fixture_pak(
            &packed.join("data.pak"),
            &[("Packed.c", b"class Packed {}")],
        );
        write_fixture_pak(
            &loose.join("data.pak"),
            &[("LoosePacked.c", b"class LoosePacked {}")],
        );
        fs::write(loose.join("scripts/Loose.c"), "class Loose {}").unwrap();
        let graph = root.join("graph.json");
        fs::write(&graph, format!(
            r#"{{"schema":"reforger-workbench-loaded-addon-graph-v1","bridgeVersion":"1.52.0","protocolVersion":1,"addons":[{{"guid":"1111111111111111","id":"Packed","title":"Packed","sourceRoot":{}}},{{"guid":"2222222222222222","id":"Loose","title":"Loose","sourceRoot":{}}}]}}"#,
            serde_json::to_string(&packed).unwrap(),
            serde_json::to_string(&loose).unwrap(),
        )).unwrap();
        fs::write(loose.join("addon.gproj"), "{}").unwrap();
        let storage = root.join("indexes");

        let workspace_roots = Vec::new();
        let first = load_or_build_loaded_addon_indexes(
            &graph,
            &storage,
            &workspace_roots,
            &IndexBuildControl::default(),
        )
        .unwrap();
        assert_eq!(first.summary.files, 3);
        assert_eq!(first.rebuilt_instances, 2);
        assert_eq!(first.instances.len(), 2);
        assert!(first
            .instances
            .iter()
            .all(|instance| instance.cache_status == "rebuilt"));
        assert!(first
            .instances
            .iter()
            .all(|instance| instance.cache_detail.as_deref() == Some("cache-missing")));
        assert_eq!(
            fs::read_dir(&storage)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.path().is_dir())
                .count(),
            2
        );
        assert!(storage.join(ADDON_CACHE_CATALOGUE_FILE).is_file());
        let readonly_loose_cache = storage
            .join(addon_instance_key(
                "2222222222222222",
                &fs::canonicalize(&loose).unwrap(),
            ))
            .join("symbols.bin");
        let stale_cache = storage.join("stale-instance");
        fs::create_dir_all(&stale_cache).unwrap();
        fs::write(stale_cache.join("keep.txt"), "read only").unwrap();
        let readonly = read_cached_loaded_addon_indexes(
            &graph,
            &storage,
            &[loose.join("scripts")],
            &IndexBuildControl::default(),
        )
        .unwrap();
        assert_eq!(readonly.loaded_instances, 1);
        assert_eq!(readonly.workspace_excluded_instances, 1);
        assert!(readonly_loose_cache.is_file());
        assert!(stale_cache.join("keep.txt").is_file());
        fs::remove_dir_all(stale_cache).unwrap();
        let manifests = fs::read_dir(&storage)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_dir())
            .map(|entry| {
                let path = entry.path().join("manifest.json");
                let bytes = fs::read(&path).unwrap();
                fs::remove_file(&path).unwrap();
                (path, bytes)
            })
            .collect::<Vec<_>>();
        let optimistic = load_cached_loaded_addon_indexes(
            &graph,
            &storage,
            &workspace_roots,
            &IndexBuildControl::default(),
        )
        .unwrap();
        assert_eq!(optimistic.loaded_instances, 2);
        assert_eq!(optimistic.missing_instances, 0);
        assert_eq!(optimistic.summary.files, 3);
        assert!(optimistic
            .instances
            .iter()
            .all(|instance| instance.cache_status == "optimistic-loaded"));
        let packed_cache = storage
            .join(addon_instance_key(
                "1111111111111111",
                &fs::canonicalize(&packed).unwrap(),
            ))
            .join("symbols.bin");
        let loose_cache = storage
            .join(addon_instance_key(
                "2222222222222222",
                &fs::canonicalize(&loose).unwrap(),
            ))
            .join("symbols.bin");
        let loose_cache_bytes = fs::read(&loose_cache).unwrap();
        fs::copy(&packed_cache, &loose_cache).unwrap();
        let identity_checked = load_cached_loaded_addon_indexes(
            &graph,
            &storage,
            &workspace_roots,
            &IndexBuildControl::default(),
        )
        .unwrap();
        assert_eq!(identity_checked.loaded_instances, 1);
        assert_eq!(identity_checked.missing_instances, 1);
        assert_eq!(identity_checked.unavailable_instances.len(), 1);
        assert_eq!(
            identity_checked.unavailable_instances[0].guid,
            "2222222222222222"
        );
        assert_eq!(identity_checked.unavailable_instances[0].title, "Loose");
        fs::write(&loose_cache, loose_cache_bytes).unwrap();
        for (path, bytes) in manifests {
            fs::write(path, bytes).unwrap();
        }
        let workspace_roots = vec![loose.join("scripts")];
        let second = load_or_build_loaded_addon_indexes(
            &graph,
            &storage,
            &workspace_roots,
            &IndexBuildControl::default(),
        )
        .unwrap();
        assert_eq!(second.loaded_instances, 1);
        assert_eq!(second.workspace_excluded_instances, 1);
        assert_eq!(second.instances.len(), 1);
        assert_eq!(second.instances[0].cache_status, "loaded");
        assert!(second.instances[0].cache_file_bytes.is_some());
        assert_eq!(
            fs::read_dir(&storage)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.path().is_dir())
                .count(),
            1
        );
        write_fixture_pak(
            &packed.join("data.pak"),
            &[("Packed.c", b"class ChangedPacked {}")],
        );
        let rebuilt = load_or_build_loaded_addon_indexes(
            &graph,
            &storage,
            &workspace_roots,
            &IndexBuildControl::default(),
        )
        .unwrap();
        assert_eq!(rebuilt.rebuilt_instances, 1);
        let packed_cache = only_cache_directory(&storage);
        assert!(packed_cache.join("symbols.bin").is_file());
        assert!(packed_cache.join("manifest.json").is_file());
        assert!(!packed_cache.join("revisions").exists());
        fs::write(loose.join("scripts/Loose.c"), "class ChangedLoose {}").unwrap();
        let changed = load_or_build_loaded_addon_indexes(
            &graph,
            &storage,
            &workspace_roots,
            &IndexBuildControl::default(),
        )
        .unwrap();
        assert_eq!(changed.loaded_instances, 1);
        assert_eq!(changed.rebuilt_instances, 0);
        fs::write(&graph, format!(
            r#"{{"schema":"reforger-workbench-loaded-addon-graph-v1","bridgeVersion":"1.52.0","protocolVersion":1,"addons":[{{"guid":"1111111111111111","id":"Packed","title":"Packed","sourceRoot":{}}}]}}"#,
            serde_json::to_string(&packed).unwrap(),
        )).unwrap();
        fs::write(packed.join("addon.gproj"), "{}").unwrap();
        let removed = load_or_build_loaded_addon_indexes(
            &graph,
            &storage,
            &[],
            &IndexBuildControl::default(),
        )
        .unwrap();
        assert_eq!(removed.index.files().len(), 1);
        assert_eq!(
            fs::read_dir(&storage)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.path().is_dir())
                .count(),
            1
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn virtual_source_uri_preserves_the_addon_and_logical_path() {
        assert_eq!(
            virtual_source_uri(BASE_GAME_GUID, "abc123", "scripts/Game/My File.c").unwrap(),
            "reforger-pak://58D0FB3206B6F859/abc123/scripts/Game/My%20File.c"
        );
    }

    #[test]
    fn reads_cached_virtual_sources_with_one_revision_validation_and_archive_open() {
        let root = test_root("batch-source-read");
        let addons = root.join("addons");
        let data = addons.join("data");
        let core = addons.join("core");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&core).unwrap();
        write_fixture_pak(
            &data.join("data007.pak"),
            &[
                ("First.c", b"class First {}"),
                ("Second.c", b"class Second {}"),
                ("Lossy.c", b"class Lossy { string Value = \"\x80\"; }"),
            ],
        );
        write_fixture_pak(&core.join("data.pak"), &[("Core.c", b"class Core {}")]);
        let inventory = root.join("inventory.json");
        write_workbench_graph_fixture(&inventory, &data);
        let storage = root.join("indexes");
        load_or_build_base_game_index(&inventory, &storage, &IndexBuildControl::default()).unwrap();
        let addon_cache = only_cache_directory(&storage);
        let manifest: AddonIndexManifest =
            serde_json::from_slice(&fs::read(addon_cache.join("manifest.json")).unwrap()).unwrap();
        let uris = manifest
            .scripts
            .iter()
            .filter(|script| {
                script.logical_path.ends_with("First.c")
                    || script.logical_path.ends_with("Second.c")
                    || script.logical_path.ends_with("Lossy.c")
            })
            .map(|script| script.uri.clone())
            .collect::<Vec<_>>();

        let batch = read_cached_virtual_sources(
            &uris,
            &addon_cache.join("symbols.bin"),
            &IndexBuildControl::default(),
        )
        .unwrap();

        assert_eq!(batch.revisions_validated, 1);
        assert_eq!(batch.archives_opened, 1);
        assert_eq!(batch.sources.len(), 3);
        assert!(batch.sources.iter().all(Result::is_ok));
        let source_text = batch
            .sources
            .into_iter()
            .map(Result::unwrap)
            .collect::<Vec<_>>();
        assert!(source_text.contains(&"class First {}".to_string()));
        assert!(source_text.contains(&"class Second {}".to_string()));
        assert!(source_text.iter().any(|source| source.contains('\u{fffd}')));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn builds_and_reuses_one_durable_index_without_materializing_sources() {
        let root = test_root("cache");
        let addons = root.join("addons");
        let data = addons.join("data");
        let core = addons.join("core");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir_all(&core).unwrap();
        {
            let file = fs::File::create(addons.join("thumbnail.png")).unwrap();
            let mut encoder = png::Encoder::new(file, 2, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder
                .write_header()
                .unwrap()
                .write_image_data(&[255, 0, 0, 255, 0, 0, 255, 255])
                .unwrap();
        }
        write_fixture_pak(
            &data.join("data007.pak"),
            &[
                ("Feature.c", b"class Feature {}"),
                ("Texture.bin", b"not script data"),
            ],
        );
        write_fixture_pak(
            &core.join("data.pak"),
            &[("CoreFeature.c", b"class CoreFeature {}")],
        );
        let inventory = root.join("inventory.json");
        write_workbench_graph_fixture(&inventory, &data);
        let storage = root.join("indexes");

        let rebuilt =
            load_or_build_base_game_index(&inventory, &storage, &IndexBuildControl::default())
                .unwrap();
        assert!(matches!(
            rebuilt.cache_status,
            IndexCacheStatus::Rebuilt { .. }
        ));
        assert_eq!(rebuilt.summary.files, 2);
        let addon_cache = only_cache_directory(&storage);
        let manifest_header_path = addon_cache.join(ADDON_MANIFEST_HEADER_FILE);
        assert!(manifest_header_path.is_file());
        let manifest: AddonIndexManifest =
            serde_json::from_slice(&fs::read(addon_cache.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest.thumbnail_color.as_deref(), Some("#800080"));
        let feature_uri = manifest
            .scripts
            .iter()
            .find(|script| script.logical_path == "Root/Feature.c")
            .unwrap()
            .uri
            .clone();
        assert_eq!(
            read_virtual_source(&feature_uri).unwrap(),
            "class Feature {}"
        );
        assert_eq!(
            read_cached_virtual_source(&feature_uri, &addon_cache.join("symbols.bin")).unwrap(),
            "class Feature {}"
        );
        let editor_normalized_uri =
            feature_uri.replacen(BASE_GAME_GUID, &BASE_GAME_GUID.to_ascii_lowercase(), 1);
        assert_eq!(
            read_virtual_source(&editor_normalized_uri).unwrap(),
            "class Feature {}"
        );

        fs::write(&manifest_header_path, b"{\"corrupt\":true}").unwrap();
        let repaired_header =
            load_or_build_base_game_index(&inventory, &storage, &IndexBuildControl::default())
                .unwrap();
        assert!(matches!(
            repaired_header.cache_status,
            IndexCacheStatus::Rebuilt { .. }
        ));
        let loaded =
            load_or_build_base_game_index(&inventory, &storage, &IndexBuildControl::default())
                .unwrap();
        assert_eq!(loaded.cache_status, IndexCacheStatus::Loaded);
        {
            let file = fs::File::create(addons.join("thumbnail.png")).unwrap();
            let mut encoder = png::Encoder::new(file, 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder
                .write_header()
                .unwrap()
                .write_image_data(&[0, 255, 0, 255])
                .unwrap();
        }
        let thumbnail_changed =
            load_or_build_base_game_index(&inventory, &storage, &IndexBuildControl::default())
                .unwrap();
        assert!(matches!(
            thumbnail_changed.cache_status,
            IndexCacheStatus::Rebuilt { .. }
        ));
        assert!(addon_cache.join("manifest.json").is_file());
        assert!(addon_cache.join("symbols.bin").is_file());
        let locator_payload =
            crate::index_cache::read_index_cache_locator_section(&addon_cache.join("symbols.bin"))
                .unwrap()
                .expect("new add-on caches embed a binary locator section");
        assert!(!locator_payload.is_empty());
        fs::remove_file(addons.join("thumbnail.png")).unwrap();
        let all_cached =
            load_all_cached_addon_indexes(&storage, &[], &IndexBuildControl::default()).unwrap();
        assert_eq!(all_cached.loaded_instances, 1);
        assert_eq!(
            all_cached.instances[0].thumbnail_color.as_deref(),
            Some("#00FF00")
        );
        let base_cached =
            load_cached_base_game_indexes(&storage, &[], &IndexBuildControl::default()).unwrap();
        assert_eq!(base_cached.loaded_instances, 1);
        let mut locator_corruption = fs::read(addon_cache.join("manifest.json")).unwrap();
        let feature_name = b"Feature.c";
        let feature_name_start = locator_corruption
            .windows(feature_name.len())
            .position(|window| window == feature_name)
            .unwrap();
        locator_corruption[feature_name_start + feature_name.len() - 1] = b'd';
        fs::write(addon_cache.join("manifest.json"), locator_corruption).unwrap();
        let repaired =
            load_or_build_base_game_index(&inventory, &storage, &IndexBuildControl::default())
                .unwrap();
        assert!(matches!(
            repaired.cache_status,
            IndexCacheStatus::Rebuilt { .. }
        ));
        fs::remove_file(&manifest_header_path).unwrap();
        let legacy_header_fallback =
            load_or_build_base_game_index(&inventory, &storage, &IndexBuildControl::default())
                .unwrap();
        assert_eq!(
            legacy_header_fallback.cache_status,
            IndexCacheStatus::Loaded
        );
        assert!(!addon_cache.join("scripts").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_same_size_script_change_replaces_the_single_current_cache() {
        let root = test_root("revision");
        let addons = root.join("addons");
        fs::create_dir_all(addons.join("data")).unwrap();
        fs::create_dir_all(addons.join("core")).unwrap();
        write_fixture_pak(
            &addons.join("data/data007.pak"),
            &[("Feature.c", b"class FeatureA {}")],
        );
        write_fixture_pak(
            &addons.join("core/data.pak"),
            &[("Core.c", b"class Core {}")],
        );
        let inventory = root.join("inventory.json");
        write_workbench_graph_fixture(&inventory, &addons.join("data"));
        let storage = root.join("indexes");
        load_or_build_base_game_index(&inventory, &storage, &IndexBuildControl::default()).unwrap();
        let addon_cache = only_cache_directory(&storage);
        let first_manifest: AddonIndexManifest =
            serde_json::from_slice(&fs::read(addon_cache.join("manifest.json")).unwrap()).unwrap();

        write_fixture_pak(
            &addons.join("data/data007.pak"),
            &[("Feature.c", b"class FeatureB {}")],
        );
        let old_uri = &first_manifest
            .scripts
            .iter()
            .find(|script| script.logical_path == "Root/Feature.c")
            .unwrap()
            .uri;
        assert!(read_virtual_source(old_uri)
            .unwrap_err()
            .contains("changed on disk"));
        let cancelled = IndexBuildControl::default();
        cancelled.cancel();
        assert_eq!(
            load_or_build_base_game_index(&inventory, &storage, &cancelled).unwrap_err(),
            crate::index_build::INDEX_BUILD_CANCELLED
        );
        let after_cancel: AddonIndexManifest =
            serde_json::from_slice(&fs::read(addon_cache.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(after_cancel, first_manifest);

        let rebuilt =
            load_or_build_base_game_index(&inventory, &storage, &IndexBuildControl::default())
                .unwrap();
        assert!(matches!(
            rebuilt.cache_status,
            IndexCacheStatus::Rebuilt { .. }
        ));
        let second_manifest: AddonIndexManifest =
            serde_json::from_slice(&fs::read(addon_cache.join("manifest.json")).unwrap()).unwrap();
        assert_ne!(first_manifest.revision, second_manifest.revision);
        assert!(addon_cache.join("symbols.bin").is_file());
        assert!(!addon_cache.join("revisions").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn inventory_only_addons_are_guid_keyed_and_duplicate_guids_are_rejected() {
        let root = test_root("inventory_addons");
        let storage = root.join("indexes");
        let first = root.join("First");
        let second = root.join("Second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        fs::write(
            first.join("addon.gproj"),
            "GameProject {\n GUID \"1337C0DE5DABBEEF\"\n ID \"RHS\"\n}",
        )
        .unwrap();
        fs::write(
            second.join("addon.gproj"),
            "GameProject {\n GUID \"1337C0DE5DABBEEF\"\n ID \"Duplicate\"\n}",
        )
        .unwrap();
        write_fixture_pak(&first.join("data.pak"), &[("First.c", b"class First {}")]);
        write_fixture_pak(
            &second.join("data.pak"),
            &[("Second.c", b"class Second {}")],
        );
        let addon = |path: &Path, name: &str| InventoryAddon {
            root_kind: "user-addons".to_string(),
            directory_name: name.to_string(),
            path: path.to_path_buf(),
            project_file: Some(path.join("addon.gproj")),
            pack_files: vec![path.join("data.pak")],
        };
        let one = Inventory {
            schema: "reforger-addon-source-inventory-v1".to_string(),
            roots: Vec::new(),
            addons: vec![addon(&first, "First")],
        };
        publish_inventory_addon_manifests(&one, &storage, &IndexBuildControl::default()).unwrap();
        let manifest_path = storage.join("1337C0DE5DABBEEF/inventory.json");
        assert!(manifest_path.is_file());
        fs::write(&manifest_path, b"corrupt").unwrap();
        publish_inventory_addon_manifests(&one, &storage, &IndexBuildControl::default()).unwrap();
        assert!(
            serde_json::from_slice::<serde_json::Value>(&fs::read(&manifest_path).unwrap()).is_ok()
        );

        let duplicate = Inventory {
            schema: "reforger-addon-source-inventory-v1".to_string(),
            roots: Vec::new(),
            addons: vec![addon(&first, "First"), addon(&second, "Second")],
        };
        assert!(publish_inventory_addon_manifests(
            &duplicate,
            &storage,
            &IndexBuildControl::default()
        )
        .unwrap_err()
        .contains("Duplicate add-on GUID"));
        let same_path_duplicate = Inventory {
            schema: "reforger-addon-source-inventory-v1".to_string(),
            roots: Vec::new(),
            addons: vec![addon(&first, "First"), addon(&first, "FirstAgain")],
        };
        assert!(publish_inventory_addon_manifests(
            &same_path_duplicate,
            &storage,
            &IndexBuildControl::default()
        )
        .unwrap_err()
        .contains("Duplicate add-on GUID"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn inventory_publication_detects_same_length_script_content_changes() {
        let root = test_root("inventory_content_identity");
        let storage = root.join("indexes");
        let addon_root = root.join("Addon");
        fs::create_dir_all(&addon_root).unwrap();
        fs::write(
            addon_root.join("addon.gproj"),
            "GameProject {\n GUID \"1337C0DE5DABBEEF\"\n ID \"ContentIdentity\"\n}",
        )
        .unwrap();
        let pack = addon_root.join("data.pak");
        write_fixture_pak(&pack, &[("Feature.c", b"class First {}")]);
        let inventory = Inventory {
            schema: "reforger-addon-source-inventory-v1".to_string(),
            roots: Vec::new(),
            addons: vec![InventoryAddon {
                root_kind: "user-addons".to_string(),
                directory_name: "Addon".to_string(),
                path: addon_root.clone(),
                project_file: Some(addon_root.join("addon.gproj")),
                pack_files: vec![pack.clone()],
            }],
        };
        let control = IndexBuildControl::default();
        publish_inventory_addon_manifests(&inventory, &storage, &control).unwrap();
        let first: InventoryPublication =
            serde_json::from_slice(&fs::read(storage.join("inventory-current.json")).unwrap())
                .unwrap();
        let first_pack_bytes = fs::metadata(&pack).unwrap().len();

        write_fixture_pak(&pack, &[("Feature.c", b"class Other {}")]);
        publish_inventory_addon_manifests(&inventory, &storage, &control).unwrap();
        let second: InventoryPublication =
            serde_json::from_slice(&fs::read(storage.join("inventory-current.json")).unwrap())
                .unwrap();

        assert_eq!(first_pack_bytes, fs::metadata(&pack).unwrap().len());
        assert_ne!(first.revision, second.revision);
        let _ = fs::remove_dir_all(root);
    }

    fn test_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "reforger_addon_sources_{name}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn write_workbench_graph_fixture(path: &Path, data_root: &Path) {
        fs::write(data_root.join("ArmaReforger.gproj"), "{}").unwrap();
        fs::write(
            path,
            format!(
                r#"{{"schema":"reforger-workbench-loaded-addon-graph-v1","bridgeVersion":"1.52.0","protocolVersion":1,"addons":[{{"guid":"{BASE_GAME_GUID}","id":"ArmaReforger","title":"Arma Reforger","sourceRoot":{}}}]}}"#,
                serde_json::to_string(data_root).unwrap(),
            ),
        )
        .unwrap();
    }

    /// The cache directory holding one add-on's index, named by its GUID.
    fn cache_directory_for(storage: &Path, guid: &str) -> PathBuf {
        let prefix = format!("{guid}-");
        fs::read_dir(storage)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .find(|path| {
                path.is_dir()
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with(&prefix))
            })
            .unwrap_or_else(|| panic!("a cached index is expected for {guid}"))
    }

    /// The one add-on cache directory in an index storage root. Directory order
    /// is filesystem-defined and the catalogue file shares the root, so the
    /// cache is selected rather than taken from the first entry.
    fn only_cache_directory(storage: &Path) -> PathBuf {
        let mut directories = fs::read_dir(storage)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect::<Vec<_>>();
        assert_eq!(
            directories.len(),
            1,
            "one cached add-on is expected in {}",
            storage.display(),
        );
        directories.remove(0)
    }

    fn write_fixture_pak(path: &Path, entries: &[(&str, &[u8])]) {
        let mut table = vec![0, 4];
        table.extend_from_slice(b"Root");
        table.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        let table_length = 1
            + 1
            + 4
            + 4
            + entries
                .iter()
                .map(|(logical, _)| 1 + 1 + logical.len() + 24)
                .sum::<usize>();
        let mut offset = 12 + 8 + 28 + 8 + table_length + 8;
        for (logical, content) in entries {
            table.push(1);
            table.push(logical.len() as u8);
            table.extend_from_slice(logical.as_bytes());
            table.extend_from_slice(&(offset as u32).to_le_bytes());
            table.extend_from_slice(&(content.len() as u32).to_le_bytes());
            table.extend_from_slice(&(content.len() as u32).to_le_bytes());
            table.extend_from_slice(&[0; 8]);
            table.extend_from_slice(&0_u32.to_be_bytes());
            offset += content.len();
        }
        let data_length = entries
            .iter()
            .map(|(_, content)| content.len())
            .sum::<usize>();
        let mut bytes = b"FORM".to_vec();
        bytes.extend_from_slice(
            &((4 + 8 + 28 + 8 + table.len() + 8 + data_length) as u32).to_be_bytes(),
        );
        bytes.extend_from_slice(b"PAC1HEAD");
        bytes.extend_from_slice(&28_u32.to_be_bytes());
        bytes.extend_from_slice(&[0; 28]);
        bytes.extend_from_slice(b"FILE");
        bytes.extend_from_slice(&(table.len() as u32).to_be_bytes());
        bytes.extend_from_slice(&table);
        bytes.extend_from_slice(b"DATA");
        bytes.extend_from_slice(&(data_length as u32).to_be_bytes());
        for (_, content) in entries {
            bytes.extend_from_slice(content);
        }
        fs::write(path, bytes).unwrap();
    }
}
