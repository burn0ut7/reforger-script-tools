use crate::host_platform::process::{self, CloseMode, ProcessIdentity};
use crate::host_platform::{
    self, workbench_host, WorkbenchHost, REFORGER_GAME_APP_ID, REFORGER_TOOLS_APP_ID,
    WORKBENCH_EXECUTABLE_NAME,
};
use crate::workbench_bridge::*;
use crate::workbench_capture::{
    self, CaptureError, CaptureRegion, CapturedWindow, WorkbenchWindowList, DEFAULT_MAX_DIMENSION,
    MAX_MAX_DIMENSION, MIN_MAX_DIMENSION,
};
use schemars::JsonSchema;
use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
#[cfg(not(test))]
use std::collections::BTreeSet;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a launch waits for the started Workbench process to appear. A Wine
/// host reaches Workbench through Steam and Proton, which run their own startup
/// in front of it.
const LAUNCH_PROCESS_DEADLINE: Duration = Duration::from_secs(120);
/// How long a launch then waits for Workbench to answer the NET API.
const NET_API_READY_DEADLINE: Duration = Duration::from_secs(90);

#[derive(Debug, Clone)]
pub struct WorkbenchGatewayOptions {
    pub host: String,
    pub port: u16,
    pub status_deadline: Duration,
    pub validation_deadline: Duration,
}

impl Default for WorkbenchGatewayOptions {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 5775,
            status_deadline: Duration::from_millis(1_500),
            validation_deadline: Duration::from_secs(120),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchStatus {
    pub is_running: bool,
    pub scripts_compiled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchRequestTiming {
    pub lock_wait_ms: u64,
    pub connect_ms: u64,
    pub write_ms: u64,
    pub response_header_ms: u64,
    pub response_body_ms: u64,
    pub decode_ms: u64,
    pub total_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchDiagnosticLocation {
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_abs: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub addon: Option<String>,
    pub line: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchCompilerDiagnostic {
    pub severity: WorkbenchDiagnosticSeverity,
    pub message: String,
    pub location: WorkbenchDiagnosticLocation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum WorkbenchDiagnosticSeverity {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchValidation {
    pub profile: String,
    pub success: bool,
    pub diagnostics: Vec<WorkbenchCompilerDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchValidationPage {
    pub profile: String,
    pub success: bool,
    pub total_diagnostics: usize,
    pub diagnostics: Vec<WorkbenchCompilerDiagnostic>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkbenchFailureCode {
    ConsentRequired,
    Unavailable,
    Timeout,
    Protocol,
    WorkbenchError,
    CaptureUnavailable,
    CaptureInvalidRegion,
    CaptureTooLarge,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkbenchFailure {
    pub code: WorkbenchFailureCode,
    pub log_reference: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WorkbenchGateway {
    options: WorkbenchGatewayOptions,
    /// The host whose path space the compiler reports its file locations in.
    host: WorkbenchHost,
    request_lock: Arc<Mutex<()>>,
}

pub const WORKBENCH_BRIDGE_VERSION: &str = "1.52.13";
pub const WORKBENCH_BRIDGE_PROTOCOL_VERSION: u32 = 1;
const WORKBENCH_REQUIRED_ADDONS: &str = "58D0FB3206B6F859,5614BBCCBB55ED1C";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkbenchInstallAuthorization {
    ExistingConsent,
    UserApprovedFirstInstall,
}

#[derive(Debug, Clone)]
pub struct WorkbenchControllerOptions {
    pub gateway: WorkbenchGatewayOptions,
    /// The Wine prefix hosting Workbench, when the host does not run it
    /// natively and the extension has been pointed at a specific prefix.
    pub wine_prefix: Option<PathBuf>,
    pub user_directory: Option<PathBuf>,
    pub profile_directory: Option<PathBuf>,
    pub game_directory: Option<PathBuf>,
    pub tools_directory: Option<PathBuf>,
    pub executable: Option<PathBuf>,
}

impl Default for WorkbenchControllerOptions {
    fn default() -> Self {
        Self {
            gateway: WorkbenchGatewayOptions::default(),
            wine_prefix: None,
            user_directory: None,
            profile_directory: None,
            game_directory: None,
            tools_directory: None,
            executable: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchPathStatus {
    pub path: Option<PathBuf>,
    pub exists: bool,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ManagedBridgeStatus {
    pub installed: bool,
    pub installation_available: bool,
    pub maintenance_required: bool,
    pub installed_version: Option<String>,
    pub active_version: Option<String>,
    pub protocol_version: Option<u32>,
    pub compatible: bool,
    pub activation_required: bool,
    pub capabilities: Vec<String>,
    pub capabilities_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchOverview {
    pub game: WorkbenchPathStatus,
    pub tools: WorkbenchPathStatus,
    pub executable: WorkbenchPathStatus,
    pub profile: WorkbenchPathStatus,
    pub bridge_directory: PathBuf,
    pub enfusion_protocol_registered: bool,
    pub native: Option<WorkbenchStatus>,
    pub native_failure: Option<String>,
    pub bridge: ManagedBridgeStatus,
    pub support_log: WorkbenchPathStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchBridgeInstallResult {
    pub installed_version: String,
    pub active_version: Option<String>,
    pub protocol_version: Option<u32>,
    pub activated: bool,
    pub managed_files: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchIntegrationBootstrapResult {
    pub net_api_enabled: bool,
    pub net_api_write_performed: bool,
    pub enfusion_protocol_registered: bool,
    pub enfusion_protocol_write_performed: bool,
    pub bridge_installed: bool,
    pub bridge_version: Option<String>,
    pub bridge_changed: bool,
    pub profile_available: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchProcessStatus {
    pub is_open: bool,
    pub process_id: Option<u32>,
    pub project_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchLiveState {
    pub bridge_version: String,
    pub protocol_version: u32,
    pub mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_world_path: Option<String>,
    pub world_editor_active: bool,
    pub world_editor_module_present: bool,
    pub world_editor_api_available: bool,
    pub play_session: WorkbenchPlaySession,
    pub loaded_addons: Vec<String>,
    pub loaded_addons_truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_sub_scene: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_entity_layer_id: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_subscene_layer: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum WorkbenchPlaySession {
    Unavailable,
    Unknown,
    LikelyRunning,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchScriptActivationResult {
    pub world_saved_before_reload: bool,
    pub world_save_status: String,
    pub reload_dispatched: bool,
    pub runtime_generation: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchSaveResult {
    pub save_all_accepted: bool,
    pub world_save_accepted: bool,
    pub world_save_status: String,
    pub action_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchEditor {
    pub id: String,
    pub display_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchEditorList {
    pub editors: Vec<WorkbenchEditor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchOpenEditorResult {
    pub editor_id: String,
    pub opened: bool,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchOpenResourceResult {
    pub resource_path: String,
    pub opened: bool,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchPlaySessionResult {
    pub accepted: bool,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchProjectContext {
    pub bridge_version: String,
    pub protocol_version: u32,
    pub loaded_addons: Vec<String>,
    pub loaded_addons_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchLoadedAddon {
    pub guid: String,
    pub id: String,
    pub title: String,
    pub source_root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchLoadedAddonGraph {
    pub bridge_version: String,
    pub protocol_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_project_file: Option<PathBuf>,
    pub addons: Vec<WorkbenchLoadedAddon>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchResourceInspection {
    pub found: bool,
    pub status: String,
    pub resource_name: Option<String>,
    pub class_name: Option<String>,
    #[serde(default)]
    pub source_addons: Vec<String>,
    #[serde(default)]
    pub source_addons_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchResourceSearchHit {
    pub resource_name: String,
    pub addon_guid: String,
    pub addon_id: Option<String>,
    pub logical_path: String,
    pub name: String,
    pub extension: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchResourceSearchPage {
    pub bridge_version: String,
    pub protocol_version: u32,
    pub project_revision: String,
    pub limit: usize,
    pub results: Vec<WorkbenchResourceSearchHit>,
    pub truncated: bool,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchEntityPosition {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchEntityTransform {
    pub position: WorkbenchEntityPosition,
    pub angles: WorkbenchEntityPosition,
    pub scale: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchEntityTransformResult {
    pub bridge_version: String,
    pub protocol_version: u32,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity: Option<WorkbenchSelectedEntity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transform: Option<WorkbenchEntityTransform>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchHistoryResult {
    pub bridge_version: String,
    pub protocol_version: u32,
    pub operation: String,
    pub status: String,
    pub history_available: bool,
    pub changed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchShapePoints {
    pub bridge_version: String,
    pub protocol_version: u32,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity: Option<WorkbenchSelectedEntity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shape_class: Option<String>,
    pub closed: bool,
    pub points: Vec<WorkbenchEntityPosition>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum WorkbenchSplineTangentMode {
    Auto,
    Explicit,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchSplineAnchor {
    pub index: usize,
    pub position: WorkbenchEntityPosition,
    pub tangent_mode: WorkbenchSplineTangentMode,
    pub in_tangent: WorkbenchEntityPosition,
    pub out_tangent: WorkbenchEntityPosition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkbenchSplineTangentModeInput {
    Auto,
    Explicit,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkbenchSplineAnchorInput {
    pub position: WorkbenchEntityPosition,
    pub tangent_mode: WorkbenchSplineTangentModeInput,
    pub in_tangent: Option<WorkbenchEntityPosition>,
    pub out_tangent: Option<WorkbenchEntityPosition>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchSpline {
    pub bridge_version: String,
    pub protocol_version: u32,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity: Option<WorkbenchSelectedEntity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shape_class: Option<String>,
    pub closed: bool,
    pub anchor_count: usize,
    pub anchors: Vec<WorkbenchSplineAnchor>,
    pub samples: Vec<WorkbenchEntityPosition>,
    pub sample_space: String,
    pub sample_count: usize,
    pub path_length: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkbenchShapePointEdit {
    Set,
    Insert,
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkbenchShapePointSpace {
    Local,
    World,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkbenchShapeTransformOperation {
    Translate,
    RotateXz,
    Scale,
    Mirror,
    Reverse,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchShapePointConversion {
    pub bridge_version: String,
    pub protocol_version: u32,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity: Option<WorkbenchSelectedEntity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shape_class: Option<String>,
    pub from_space: String,
    pub to_space: String,
    pub points: Vec<WorkbenchEntityPosition>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchPolylineResample {
    pub bridge_version: String,
    pub protocol_version: u32,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity: Option<WorkbenchSelectedEntity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shape_class: Option<String>,
    pub closed: bool,
    pub points: Vec<WorkbenchEntityPosition>,
    pub spacing_meters: f32,
    pub original_point_count: usize,
    pub result_point_count: usize,
    pub path_length: f32,
    pub skipped_zero_length_segments: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchSelectedEntity {
    pub entity_id: String,
    pub class_name: String,
    pub sub_scene: i32,
    pub layer_id: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sub_scene_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layer_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<WorkbenchEntityPosition>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchWorldSelectionSummary {
    pub bridge_version: String,
    pub protocol_version: u32,
    pub editor_available: bool,
    pub status: String,
    pub selected_count: u32,
    pub selected_entities: Vec<WorkbenchSelectedEntity>,
    pub selected_entities_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchSelectedEntityHierarchy {
    pub bridge_version: String,
    pub protocol_version: u32,
    pub editor_available: bool,
    pub status: String,
    pub selection_index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity: Option<WorkbenchSelectedEntity>,
    pub ancestors: Vec<WorkbenchSelectedEntity>,
    pub ancestors_truncated: bool,
    pub children: Vec<WorkbenchSelectedEntity>,
    pub children_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchEntityListPage {
    pub bridge_version: String,
    pub protocol_version: u32,
    pub world_revision: String,
    pub limit: usize,
    pub entities: Vec<WorkbenchSelectedEntity>,
    pub truncated: bool,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchEntitySearchHit {
    pub entity: WorkbenchSelectedEntity,
    pub component_classes: Vec<String>,
    /// The requested direct component classes that this entity satisfied.
    pub matched_component_classes: Vec<String>,
    pub matched_fields: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_class_name: Option<String>,
    pub child_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relation_match: Option<WorkbenchEntityRelationMatch>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum WorkbenchEntityRelationDirection {
    Parent,
    Ancestor,
    Child,
    Descendant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkbenchEntityRelationFilter {
    pub direction: WorkbenchEntityRelationDirection,
    pub class_name: Option<String>,
    pub component_classes: Vec<String>,
    pub max_depth: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchEntityRelationMatch {
    pub direction: WorkbenchEntityRelationDirection,
    pub depth: i32,
    pub entity_id: String,
    pub class_name: String,
    pub sub_scene: i32,
    pub layer_id: i32,
    pub matched_component_classes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchEntitySearchSummary {
    /// Matches observed through the current page boundary. Exact only when `truncated` is false.
    pub total_matches: u32,
    /// Matches with an authored entity name. The remainder are anonymous authored entities.
    pub named_matches: u32,
    pub anonymous_matches: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchEntitySearchPage {
    pub bridge_version: String,
    pub protocol_version: u32,
    pub world_revision: String,
    pub status: String,
    pub limit: usize,
    pub summary: WorkbenchEntitySearchSummary,
    pub results: Vec<WorkbenchEntitySearchHit>,
    pub truncated: bool,
    /// A relation traversal hit its fixed per-candidate visit limit; affected candidates are
    /// omitted rather than searched without a bound.
    pub relation_traversal_truncated: bool,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchLayerState {
    pub bridge_version: String,
    pub protocol_version: u32,
    pub status: String,
    pub sub_scene: i32,
    pub layer_id: i32,
    pub layer_path: String,
    pub visible: bool,
    pub explicitly_locked: bool,
    pub locked_in_hierarchy: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchEntityInspection {
    #[serde(skip_serializing)]
    pub bridge_version: String,
    #[serde(skip_serializing)]
    pub protocol_version: u32,
    #[serde(skip_serializing)]
    pub editor_available: bool,
    #[serde(skip_serializing_if = "is_available_status")]
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity: Option<WorkbenchSelectedEntity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_name: Option<String>,
    pub resource_reference_kind: String,
    pub contributor_addons: Vec<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub contributor_addons_truncated: bool,
    pub ancestors: Vec<WorkbenchSelectedEntity>,
    #[serde(skip_serializing_if = "is_false")]
    pub ancestors_truncated: bool,
    pub children: Vec<WorkbenchSelectedEntity>,
    #[serde(skip_serializing_if = "is_false")]
    pub children_truncated: bool,
    pub components: Vec<WorkbenchComponent>,
    #[serde(skip_serializing_if = "is_false")]
    pub component_properties_truncated: bool,
    pub properties: Vec<WorkbenchPrefabProperty>,
    #[serde(skip_serializing_if = "is_false")]
    pub properties_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchEntitySelectionResult {
    pub bridge_version: String,
    pub protocol_version: u32,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity: Option<WorkbenchSelectedEntity>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchEntityRadiusQuery {
    pub bridge_version: String,
    pub protocol_version: u32,
    pub status: String,
    pub center: WorkbenchEntityPosition,
    pub radius_meters: f32,
    pub query_scope: String,
    pub require_object: bool,
    pub exclude_proxies: bool,
    pub entities: Vec<WorkbenchSelectedEntity>,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct WorkbenchEntityRadiusQueryOptions {
    pub center: WorkbenchEntityPosition,
    pub radius_meters: f32,
    pub query_scope: String,
    pub require_object: bool,
    pub exclude_proxies: bool,
    pub class_name: Option<String>,
    pub limit: usize,
}

#[derive(Debug, Clone)]
pub struct WorkbenchTerrainSampleOptions {
    pub center_x: f32,
    pub center_z: f32,
    pub half_extent_meters: f32,
    pub spacing_meters: Option<f32>,
    pub include_water: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchTerrainCoordinate {
    pub x: f32,
    pub z: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchTerrainBounds {
    pub min: WorkbenchEntityPosition,
    pub max: WorkbenchEntityPosition,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchTerrainMetadata {
    pub bounds: WorkbenchTerrainBounds,
    pub heightmap_resolution_x: u32,
    pub heightmap_resolution_z: u32,
    pub native_spacing_meters: f32,
    pub tile_count_x: u32,
    pub tile_count_z: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchTerrainGrid {
    pub origin: WorkbenchTerrainCoordinate,
    pub requested_half_extent_meters: f32,
    pub requested_spacing_meters: Option<f32>,
    pub effective_spacing_meters: f32,
    pub spacing_clamped: bool,
    pub width: u32,
    pub height: u32,
    pub heights: Vec<Option<f32>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum WorkbenchTerrainWaterType {
    None,
    Ocean,
    Pond,
    River,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchTerrainWaterGrid {
    /// `null` marks an absent terrain cell; `none` marks dry valid terrain.
    pub types: Vec<Option<WorkbenchTerrainWaterType>>,
    pub surface_heights: Vec<Option<f32>>,
    pub depths_above_terrain: Vec<Option<f32>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchTerrainWaterSummary {
    pub wet_sample_count: u32,
    pub ocean_sample_count: u32,
    pub pond_sample_count: u32,
    pub river_sample_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maximum_depth_above_terrain: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchTerrainSummary {
    pub valid_sample_count: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimum_height: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maximum_height: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mean_height: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elevation_range: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steepest_adjacent_slope_degrees: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steepest_adjacent_slope_position: Option<WorkbenchTerrainCoordinate>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchTerrainSample {
    pub bridge_version: String,
    pub protocol_version: u32,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terrain: Option<WorkbenchTerrainMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grid: Option<WorkbenchTerrainGrid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<WorkbenchTerrainSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub water: Option<WorkbenchTerrainWaterGrid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub water_summary: Option<WorkbenchTerrainWaterSummary>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchViewportContext {
    pub bridge_version: String,
    pub protocol_version: u32,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mouse_x: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mouse_y: Option<i32>,
    pub mouse_inside: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub camera_position: Option<WorkbenchEntityPosition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub camera_direction: Option<WorkbenchEntityPosition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mouse_world_position: Option<WorkbenchEntityPosition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ray_start: Option<WorkbenchEntityPosition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ray_end: Option<WorkbenchEntityPosition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ray_direction: Option<WorkbenchEntityPosition>,
}

#[derive(Debug, Clone)]
pub struct WorkbenchViewportContextOptions {
    pub include_ray: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum WorkbenchTraceShape {
    Line,
    Sphere,
    Box,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum WorkbenchTraceHitKind {
    Entity,
    Terrain,
    Ocean,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchTraceOptions {
    pub start: WorkbenchEntityPosition,
    pub end: WorkbenchEntityPosition,
    pub shape: WorkbenchTraceShape,
    pub radius: Option<f32>,
    pub box_mins: Option<WorkbenchEntityPosition>,
    pub box_maxs: Option<WorkbenchEntityPosition>,
    pub entities: bool,
    pub terrain: bool,
    pub ocean: bool,
    pub target_layers: Option<i32>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchTraceResult {
    pub bridge_version: String,
    pub protocol_version: u32,
    pub status: String,
    pub hit: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fraction: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<WorkbenchEntityPosition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub normal: Option<WorkbenchEntityPosition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<WorkbenchTraceHitKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity: Option<WorkbenchSelectedEntity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collider_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub material: Option<String>,
}

const MAX_WORKBENCH_TRACE_LENGTH_METERS: f32 = 10_000.0;
const MAX_WORKBENCH_TRACE_DIMENSION_METERS: f32 = 1_000.0;
const MAX_WORKBENCH_TARGET_LAYER_MASK: i32 = 0x7fff_ffff;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchEntityMutationResult {
    pub bridge_version: String,
    pub protocol_version: u32,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_layer_id: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity: Option<WorkbenchSelectedEntity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirmation_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub destination_exists: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub persistence_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template_saved: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inspection: Option<WorkbenchPrefabContext>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchPrefabResourceMutationResult {
    pub bridge_version: String,
    pub protocol_version: u32,
    pub status: String,
    pub resource_name: String,
    pub persistence_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub component_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub component_class: Option<String>,
    pub template_saved: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inspection: Option<WorkbenchPrefabContext>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub component_inspection: Option<WorkbenchPrefabComponentInspection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirmation_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchComponent {
    pub component_id: String,
    pub class_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub property_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub direct_override_count: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchComponentResult {
    pub bridge_version: String,
    pub protocol_version: u32,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity: Option<WorkbenchSelectedEntity>,
    pub components: Vec<WorkbenchComponent>,
    pub properties: Vec<WorkbenchDirectProperty>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirmation_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchDirectProperty {
    pub name: String,
    pub data_type: String,
    pub value: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directly_overridden: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value_origin: Option<WorkbenchPrefabPropertyOrigin>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_descriptor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchPropertyList {
    pub bridge_version: String,
    pub protocol_version: u32,
    pub status: String,
    pub properties: Vec<WorkbenchDirectProperty>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum WorkbenchPrefabPropertyOrigin {
    Direct,
    Inherited,
    Default,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchPrefabProperty {
    pub path: String,
    pub data_type: String,
    pub value: Value,
    pub directly_overridden: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value_origin: Option<WorkbenchPrefabPropertyOrigin>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_descriptor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchPrefabComponent {
    pub component_id: String,
    pub class_name: String,
    pub properties: Vec<WorkbenchPrefabProperty>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchPrefabComponentInspection {
    pub bridge_version: String,
    pub protocol_version: u32,
    pub status: String,
    pub resource_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub member_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub component: Option<WorkbenchPrefabComponent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchPrefabMember {
    /// Stable only within this one inspected prefab context, not a live entity identity.
    pub member_id: String,
    pub class_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchPrefabContext {
    pub bridge_version: String,
    pub protocol_version: u32,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity: Option<WorkbenchSelectedEntity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_reference_kind: Option<String>,
    pub contributor_addons: Vec<String>,
    pub ancestor_resources: Vec<String>,
    pub ancestor_resources_truncated: bool,
    pub prefab_edit_mode: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub member_id: Option<String>,
    pub components: Vec<WorkbenchComponent>,
    /// Direct stored prefab members only; these are not live scene entity identities.
    pub children: Vec<WorkbenchPrefabMember>,
    pub children_truncated: bool,
    pub properties: Vec<WorkbenchPrefabProperty>,
    pub properties_truncated: bool,
    pub child_count: u32,
}

#[derive(Debug, Clone)]
struct PropertyWriteDescriptor {
    entity_id: String,
    component_id: Option<String>,
    property_name: String,
    data_type: String,
    observed_value: String,
    issued: Instant,
}

#[derive(Debug, Clone)]
pub struct WorkbenchCreateEntityOptions {
    pub target: String,
    pub target_is_resource: bool,
    pub sub_scene: i32,
    pub position: WorkbenchEntityPosition,
    pub angles: WorkbenchEntityPosition,
    pub layer_id: i32,
    pub name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchLogRead {
    pub source: String,
    pub path: Option<PathBuf>,
    pub lines: Vec<String>,
    pub markers: Vec<WorkbenchLogMarker>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchLogMarker {
    pub kind: String,
    pub line_index: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkbenchProcessResult {
    pub process_id: Option<u32>,
    pub already_running: bool,
    pub net_api_connected: bool,
    pub exited: bool,
    pub user_interaction_required: bool,
}

#[derive(Debug, Clone)]
pub struct WorkbenchController {
    options: WorkbenchControllerOptions,
    /// The host this controller addresses Workbench on. Resolved once so that
    /// every path this controller exchanges with Workbench uses one mapping.
    host: WorkbenchHost,
    gateway: WorkbenchGateway,
    observed_processes: Arc<Mutex<HashSet<ProcessIdentity>>>,
    validation_snapshot: Arc<Mutex<Option<(String, WorkbenchValidation)>>>,
    maintenance_lock: Arc<Mutex<()>>,
    capture_lock: Arc<Mutex<()>>,
    delete_confirmations: Arc<Mutex<HashMap<String, (String, Instant)>>>,
    property_write_descriptors: Arc<Mutex<HashMap<String, PropertyWriteDescriptor>>>,
}

impl WorkbenchController {
    pub fn new(options: WorkbenchControllerOptions) -> Self {
        let host = match options.wine_prefix.as_deref() {
            Some(prefix) => WorkbenchHost::detect(Some(prefix)),
            None => workbench_host().clone(),
        };
        Self::with_host(options, host)
    }

    fn with_host(options: WorkbenchControllerOptions, host: WorkbenchHost) -> Self {
        let gateway = WorkbenchGateway::with_host(options.gateway.clone(), host.clone());
        Self {
            options,
            host,
            gateway,
            observed_processes: Arc::new(Mutex::new(HashSet::new())),
            validation_snapshot: Arc::new(Mutex::new(None)),
            maintenance_lock: Arc::new(Mutex::new(())),
            capture_lock: Arc::new(Mutex::new(())),
            delete_confirmations: Arc::new(Mutex::new(HashMap::new())),
            property_write_descriptors: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn correlate_failure(
        &self,
        operation: &str,
        failure: WorkbenchFailure,
    ) -> WorkbenchFailure {
        if failure.log_reference.is_some() {
            return failure;
        }
        let code = failure_code(failure.code);
        self.correlate_failure_details(
            operation,
            code,
            failure,
            json!({
                "failureCode": code,
            }),
        )
    }

    pub fn overview(&self) -> WorkbenchOverview {
        let started = Instant::now();
        let paths = self.paths();
        let native_result = self.gateway.status();
        let native_failure = native_result
            .as_ref()
            .err()
            .map(|failure| failure_code(failure.code).to_string());
        let native = native_result.ok();
        let mut bridge = self.bridge_disk_status(&paths.bridge_directory);
        let enfusion_protocol_registered = enfusion_protocol_registered(&self.host, &paths);
        if native.is_some() {
            if !bridge.installed {
                bridge.installation_available = paths.profile.is_dir();
            }
        }
        let mut overview = WorkbenchOverview {
            game: path_status(paths.game, &paths.game_source),
            tools: path_status(paths.tools, &paths.tools_source),
            executable: path_status(paths.executable, &paths.executable_source),
            profile: path_status(Some(paths.profile), paths.profile_source),
            bridge_directory: paths.bridge_directory,
            enfusion_protocol_registered,
            native,
            native_failure,
            bridge,
            support_log: path_status(Some(self.integration_log_path()), "host-support-log"),
        };
        self.log_event_timed(
            "status",
            if overview.native.is_some() {
                "connected"
            } else {
                "unavailable"
            },
            started,
            json!({
                "gameFound": overview.game.exists,
                "toolsFound": overview.tools.exists,
                "executableFound": overview.executable.exists,
                "profileFound": overview.profile.exists,
                "bridgeInstalled": overview.bridge.installed,
                "bridgeVersion": overview.bridge.installed_version.clone(),
                "protocolVersion": overview.bridge.protocol_version,
                "enfusionProtocolRegistered": overview.enfusion_protocol_registered,
            }),
        );
        overview.support_log.exists = overview
            .support_log
            .path
            .as_ref()
            .is_some_and(|path| path.is_file());
        overview
    }

    pub fn native_status(&self) -> Result<WorkbenchStatus, WorkbenchFailure> {
        self.native_status_with_timing().map(|(status, _)| status)
    }

    pub fn native_status_with_timing(
        &self,
    ) -> Result<(WorkbenchStatus, WorkbenchRequestTiming), WorkbenchFailure> {
        let started = Instant::now();
        let result = self.gateway.status_with_timing();
        self.log_event_timed(
            "native-status",
            match &result {
                Ok(_) => "connected",
                Err(failure) => failure_code(failure.code),
            },
            started,
            json!({
                "isRunning": result.as_ref().map(|(status, _)| status.is_running).ok(),
                "scriptsCompiled": result.as_ref().map(|(status, _)| status.scripts_compiled).ok(),
            }),
        );
        result
    }

    pub fn bootstrap_integration(
        &self,
    ) -> Result<WorkbenchIntegrationBootstrapResult, WorkbenchFailure> {
        let _maintenance = self
            .maintenance_lock
            .lock()
            .map_err(|_| failure(WorkbenchFailureCode::Unavailable))?;
        let started = Instant::now();
        let paths = self.paths();
        let enfusion_protocol_write_performed = register_enfusion_protocol(&self.host, &paths)
            .map_err(|error| {
                self.registry_write_failure("enfusion-protocol-registration-failed", &error)
            })?;
        let net_api_write_performed = enable_workbench_net_api(&self.host)
            .map_err(|error| self.registry_write_failure("net-api-enable-failed", &error))?;
        let result = self.prepare_bridge_locked(true)?;
        self.log_event_timed(
            "integration-bootstrap",
            "ready",
            started,
            json!({
                "netApiEnabled": true,
                "netApiWritePerformed": net_api_write_performed,
                "enfusionProtocolRegistered": true,
                "enfusionProtocolWritePerformed": enfusion_protocol_write_performed,
                "bridgeInstalled": result.bridge_installed,
                "bridgeVersion": result.bridge_version.clone(),
                "bridgeChanged": result.bridge_changed,
                "profileAvailable": result.profile_available,
            }),
        );
        Ok(WorkbenchIntegrationBootstrapResult {
            net_api_enabled: true,
            net_api_write_performed,
            enfusion_protocol_registered: true,
            enfusion_protocol_write_performed,
            ..result
        })
    }

    /// Reports a failed write to the registry Workbench reads its options from.
    ///
    /// A prefix that a wineserver is holding is recorded as its own outcome:
    /// the write is refused rather than made and discarded, and the user has to
    /// close Workbench before setup can complete.
    fn registry_write_failure(&self, outcome: &str, error: &std::io::Error) -> WorkbenchFailure {
        let busy = error.kind() == std::io::ErrorKind::ResourceBusy;
        self.correlate_failure_details(
            "integration-bootstrap",
            if busy { "wine-prefix-in-use" } else { outcome },
            failure(WorkbenchFailureCode::Unavailable),
            json!({
                "errorKind": format!("{:?}", error.kind()),
                "operation": outcome,
                "workbenchHost": self.host.source(),
            }),
        )
    }

    pub fn maintain_integration(
        &self,
    ) -> Result<WorkbenchIntegrationBootstrapResult, WorkbenchFailure> {
        let _maintenance = self
            .maintenance_lock
            .lock()
            .map_err(|_| failure(WorkbenchFailureCode::Unavailable))?;
        let result = self.prepare_bridge_locked(true)?;
        Ok(WorkbenchIntegrationBootstrapResult {
            net_api_enabled: false,
            net_api_write_performed: false,
            enfusion_protocol_registered: false,
            enfusion_protocol_write_performed: false,
            ..result
        })
    }

    pub fn process_status(&self) -> WorkbenchProcessStatus {
        let process = process::workbench_processes().into_iter().next();
        let process_id = process.as_ref().map(|value| value.id);
        let project_path = process.and_then(|process| workbench_project_gproj(&self.host, process));
        WorkbenchProcessStatus {
            is_open: process_id.is_some(),
            process_id,
            project_path,
        }
    }

    pub fn launch_default_project(&self) -> Result<WorkbenchProcessResult, WorkbenchFailure> {
        let project = self
            .paths()
            .game
            .as_ref()
            .map(|game| game.join("addons").join("data").join("ArmaReforger.gproj"))
            .filter(|project| project.is_file())
            .ok_or_else(|| {
                self.correlate_failure_details(
                    "launch-default",
                    "default-project-unavailable",
                    failure(WorkbenchFailureCode::Unavailable),
                    json!({"projectFound": false}),
                )
            })?;
        self.launch(&project)
    }

    fn prepare_bridge_locked(
        &self,
        allow_first_install: bool,
    ) -> Result<WorkbenchIntegrationBootstrapResult, WorkbenchFailure> {
        let paths = self.paths();
        if !paths.profile.is_dir() {
            return Ok(WorkbenchIntegrationBootstrapResult {
                net_api_enabled: false,
                net_api_write_performed: false,
                enfusion_protocol_registered: false,
                enfusion_protocol_write_performed: false,
                bridge_installed: false,
                bridge_version: None,
                bridge_changed: false,
                profile_available: false,
            });
        }
        let manifest_path = paths
            .bridge_directory
            .join("reforger-script-tools.manifest.json");
        let mut existing_manifest = fs::read(&manifest_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<BridgeManifest>(&bytes).ok());
        if existing_manifest.is_none()
            && self
                .migrate_legacy_bridge(&paths.legacy_bridge_directory, &paths.bridge_directory)
                .unwrap_or(false)
        {
            existing_manifest = fs::read(&manifest_path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<BridgeManifest>(&bytes).ok());
        }
        if existing_manifest.is_none() && !allow_first_install {
            return Err(self.correlate_failure_details(
                "integration-maintenance",
                "consent-required",
                failure(WorkbenchFailureCode::ConsentRequired),
                json!({"managedDirectoryCreated": false, "manifestFound": false}),
            ));
        }
        let bridge_changed = self.bridge_needs_maintenance(&paths.bridge_directory);
        if bridge_changed {
            self.write_managed_files(&paths.bridge_directory)
                .map_err(|error| {
                    self.correlate_failure_details(
                        "integration-maintenance",
                        "write-failed",
                        failure(WorkbenchFailureCode::Unavailable),
                        json!({
                            "errorKind": format!("{:?}", error.kind()),
                            "managedFileCount": bridge_payload().len(),
                        }),
                    )
                })?;
            existing_manifest = fs::read(&manifest_path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<BridgeManifest>(&bytes).ok());
        }
        Ok(WorkbenchIntegrationBootstrapResult {
            net_api_enabled: false,
            net_api_write_performed: false,
            enfusion_protocol_registered: false,
            enfusion_protocol_write_performed: false,
            bridge_installed: existing_manifest.is_some(),
            bridge_version: existing_manifest.map(|manifest| manifest.bridge_version),
            bridge_changed,
            profile_available: true,
        })
    }

    pub fn install_bridge(
        &self,
        authorization: WorkbenchInstallAuthorization,
    ) -> Result<WorkbenchBridgeInstallResult, WorkbenchFailure> {
        let _maintenance = self
            .maintenance_lock
            .lock()
            .map_err(|_| failure(WorkbenchFailureCode::Unavailable))?;
        let started = Instant::now();
        if let Err(failure) = self.gateway.status() {
            return Err(self.correlate_failure_details(
                "install",
                "native-unavailable",
                failure,
                json!({"nativeConnected": false}),
            ));
        }
        let paths = self.paths();
        let prepared = self.prepare_bridge_locked(
            authorization == WorkbenchInstallAuthorization::UserApprovedFirstInstall,
        )?;
        if !prepared.profile_available {
            return Err(self.correlate_failure_details(
                "install",
                "profile-missing",
                failure(WorkbenchFailureCode::Unavailable),
                json!({
                    "profileFound": false,
                    "managedDirectoryCreated": false,
                }),
            ));
        }
        let existing_manifest = fs::read(
            paths
                .bridge_directory
                .join("reforger-script-tools.manifest.json"),
        )
        .ok()
        .and_then(|bytes| serde_json::from_slice::<BridgeManifest>(&bytes).ok());
        if let Some(manifest) = existing_manifest.as_ref().filter(|manifest| {
            version_order(&manifest.bridge_version, WORKBENCH_BRIDGE_VERSION).is_gt()
        }) {
            let active = self.active_bridge_status(&paths.bridge_directory, true);
            let result = WorkbenchBridgeInstallResult {
                installed_version: manifest.bridge_version.clone(),
                active_version: active.active_version,
                protocol_version: active.protocol_version,
                activated: active.compatible && !active.activation_required,
                managed_files: manifest.files.len(),
            };
            self.log_event_timed(
                "install",
                "newer-preserved",
                started,
                json!({
                    "bridgeVersion": result.installed_version.clone(),
                    "activeVersion": result.active_version.clone(),
                    "protocolVersion": result.protocol_version,
                    "managedFileCount": result.managed_files,
                    "managedFiles": manifest
                        .files
                        .iter()
                        .map(|file| file.name.as_str())
                        .collect::<Vec<_>>(),
                    "activated": result.activated,
                }),
            );
            return Ok(result);
        }
        let validation = self.gateway.validate_scripts();
        let result = WorkbenchBridgeInstallResult {
            installed_version: WORKBENCH_BRIDGE_VERSION.to_string(),
            // Profile NetApiHandler discovery happens when Workbench reloads its
            // script runtime. Do not call the newly written handler here: until
            // that reload, it cannot exist and the probe only adds a misleading
            // NET API error to the Workbench log.
            active_version: None,
            protocol_version: None,
            activated: false,
            managed_files: bridge_payload().len(),
        };
        self.log_event_timed(
            "install",
            "installed",
            started,
            json!({
                "bridgeVersion": result.installed_version.clone(),
                "activeVersion": result.active_version.clone(),
                "protocolVersion": result.protocol_version,
                "managedFileCount": result.managed_files,
                "managedFiles": bridge_payload()
                    .iter()
                    .map(|(name, _)| *name)
                    .collect::<Vec<_>>(),
                "activated": result.activated,
                "validationSuccess": validation.as_ref().ok().map(|value| value.success),
            }),
        );
        Ok(result)
    }

    pub fn validate_scripts(&self) -> Result<WorkbenchValidation, WorkbenchFailure> {
        self.native_validate_scripts()
    }

    pub fn native_validate_scripts(&self) -> Result<WorkbenchValidation, WorkbenchFailure> {
        let started = Instant::now();
        let result = self.gateway.validate_scripts();
        self.log_event_timed(
            "validate",
            match &result {
                Ok(validation) if validation.success => "success",
                Ok(_) => "compiler-findings",
                Err(_) => "failed",
            },
            started,
            json!({
                "diagnosticCount": result.as_ref().map(|value| value.diagnostics.len()).ok(),
                "success": result.as_ref().map(|value| value.success).ok(),
            }),
        );
        result
    }

    pub fn validate_scripts_page(
        &self,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<WorkbenchValidationPage, WorkbenchFailure> {
        let limit = limit.clamp(1, 200);
        let (token, validation, offset) = if let Some(cursor) = cursor {
            let (token, offset) = parse_validation_cursor(cursor)
                .ok_or_else(|| failure(WorkbenchFailureCode::Protocol))?;
            let snapshot = self
                .validation_snapshot
                .lock()
                .ok()
                .and_then(|snapshot| snapshot.clone())
                .filter(|(snapshot_token, _)| snapshot_token == &token)
                .ok_or_else(|| failure(WorkbenchFailureCode::Unavailable))?;
            (snapshot.0, snapshot.1, offset)
        } else {
            let validation = self.validate_scripts()?;
            let token = sha256(
                &serde_json::to_vec(&validation)
                    .map_err(|_| failure(WorkbenchFailureCode::Protocol))?,
            );
            if let Ok(mut snapshot) = self.validation_snapshot.lock() {
                *snapshot = Some((token.clone(), validation.clone()));
            }
            (token, validation, 0)
        };
        if offset > validation.diagnostics.len() {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        let end = (offset + limit).min(validation.diagnostics.len());
        let next_cursor =
            (end < validation.diagnostics.len()).then(|| format!("wv1:{token}:{end}"));
        Ok(WorkbenchValidationPage {
            profile: validation.profile,
            success: validation.success,
            total_diagnostics: validation.diagnostics.len(),
            diagnostics: validation.diagnostics[offset..end].to_vec(),
            next_cursor,
        })
    }

    pub fn state(&self) -> Result<WorkbenchLiveState, WorkbenchFailure> {
        let started = Instant::now();
        let value = self
            .gateway
            .request(
                json!({"APIFunc": "RST_WorkbenchState"}),
                self.options.gateway.status_deadline,
            )
            .map_err(|failure| {
                self.correlate_failure_details(
                    "state",
                    failure_code(failure.code),
                    failure,
                    json!({"handler": "RST_WorkbenchState"}),
                )
            })?;
        let raw: RawBridgeState = serde_json::from_value(value).map_err(|_| {
            self.correlate_failure_details(
                "state",
                "workbench_protocol_error",
                failure(WorkbenchFailureCode::Protocol),
                json!({"handler": "RST_WorkbenchState"}),
            )
        })?;
        if raw.bridge_version != WORKBENCH_BRIDGE_VERSION
            || raw.protocol_version != WORKBENCH_BRIDGE_PROTOCOL_VERSION
        {
            return Err(self.correlate_failure_details(
                "state",
                "incompatible-handler",
                failure(WorkbenchFailureCode::Protocol),
                json!({
                    "handler": "RST_WorkbenchState",
                    "expectedProtocolVersion": WORKBENCH_BRIDGE_PROTOCOL_VERSION,
                    "activeProtocolVersion": raw.protocol_version,
                    "activeBridgeVersion": raw.bridge_version,
                }),
            ));
        }
        let (loaded_addons, loaded_addons_truncated) =
            split_bounded_list(&raw.loaded_addons, 256, 256);
        let world_editor_module_present = workbench_bool(&raw.world_editor_module_present);
        let world_editor_api_available = workbench_bool(&raw.world_editor_api_available)
            || workbench_bool(&raw.world_editor_active)
            || raw.mode == "world-editor";
        let state = WorkbenchLiveState {
            bridge_version: raw.bridge_version,
            protocol_version: raw.protocol_version,
            world_editor_active: world_editor_api_available,
            world_editor_module_present,
            world_editor_api_available,
            play_session: play_session(
                &raw.play_session,
                world_editor_module_present,
                world_editor_api_available,
            )
            .ok_or_else(|| {
                self.correlate_failure_details(
                    "state",
                    "workbench_protocol_error",
                    failure(WorkbenchFailureCode::Protocol),
                    json!({"handler": "RST_WorkbenchState"}),
                )
            })?,
            mode: raw.mode,
            active_world_path: raw.active_world_path.filter(|path| !path.is_empty()),
            loaded_addons,
            loaded_addons_truncated,
            current_sub_scene: raw.current_sub_scene,
            current_entity_layer_id: raw.current_entity_layer_id,
            active_subscene_layer: raw.active_subscene_layer.filter(|layer| !layer.is_empty()),
        };
        self.log_event_timed(
            "state",
            "success",
            started,
            json!({
                "activeBridgeVersion": state.bridge_version.clone(),
                "protocolVersion": state.protocol_version,
                "mode": state.mode.clone(),
                "worldEditorActive": state.world_editor_active,
                "worldEditorModulePresent": state.world_editor_module_present,
                "worldEditorApiAvailable": state.world_editor_api_available,
                "playSession": state.play_session,
                "loadedAddonCount": state.loaded_addons.len(),
                "loadedAddonsTruncated": state.loaded_addons_truncated,
            }),
        );
        Ok(state)
    }

    pub fn list_editors(&self) -> Result<WorkbenchEditorList, WorkbenchFailure> {
        let started = Instant::now();
        let value = self
            .gateway
            .request(
                json!({"APIFunc": "RST_WorkbenchListEditors"}),
                self.options.gateway.status_deadline,
            )
            .map_err(|failure| {
                self.correlate_failure_details(
                    "list_editors",
                    failure_code(failure.code),
                    failure,
                    json!({"handler": "RST_WorkbenchListEditors"}),
                )
            })?;
        let raw: RawWorkbenchEditorList = serde_json::from_value(value).map_err(|_| {
            self.correlate_failure_details(
                "list_editors",
                "workbench_protocol_error",
                failure(WorkbenchFailureCode::Protocol),
                json!({"handler": "RST_WorkbenchListEditors"}),
            )
        })?;
        let editors = raw
            .editors
            .split(';')
            .filter_map(|entry| {
                let (id, display_name) = entry.split_once('|')?;
                (!id.is_empty() && !display_name.is_empty()).then(|| WorkbenchEditor {
                    id: id.to_string(),
                    display_name: display_name.to_string(),
                })
            })
            .collect::<Vec<_>>();
        if editors.is_empty() {
            return Err(self.correlate_failure_details(
                "list_editors",
                "workbench_protocol_error",
                failure(WorkbenchFailureCode::Protocol),
                json!({"handler": "RST_WorkbenchListEditors"}),
            ));
        }
        let result = WorkbenchEditorList { editors };
        self.log_event_timed(
            "list-editors",
            "success",
            started,
            json!({"editorCount": result.editors.len()}),
        );
        Ok(result)
    }

    pub fn open_editor(
        &self,
        editor_id: &str,
    ) -> Result<WorkbenchOpenEditorResult, WorkbenchFailure> {
        if editor_id.trim().is_empty() {
            return Err(self.correlate_failure_details(
                "open_editor",
                "editor-id-required",
                failure(WorkbenchFailureCode::Protocol),
                json!({}),
            ));
        }
        let started = Instant::now();
        let value = self
            .gateway
            .request(
                json!({"APIFunc": "RST_WorkbenchOpenEditor", "editorId": editor_id}),
                self.options.gateway.status_deadline,
            )
            .map_err(|failure| {
                self.correlate_failure_details(
                    "open_editor",
                    failure_code(failure.code),
                    failure,
                    json!({"handler": "RST_WorkbenchOpenEditor"}),
                )
            })?;
        let raw: RawWorkbenchOpenEditor = serde_json::from_value(value).map_err(|_| {
            self.correlate_failure_details(
                "open_editor",
                "workbench_protocol_error",
                failure(WorkbenchFailureCode::Protocol),
                json!({"handler": "RST_WorkbenchOpenEditor"}),
            )
        })?;
        let result = WorkbenchOpenEditorResult {
            editor_id: raw.editor_id,
            opened: workbench_bool(&raw.opened),
            status: raw.status,
        };
        self.log_event_timed(
            "open-editor",
            &result.status,
            started,
            json!({"editorId": editor_id, "opened": result.opened}),
        );
        Ok(result)
    }

    pub fn open_resource(
        &self,
        resource_path: &str,
    ) -> Result<WorkbenchOpenResourceResult, WorkbenchFailure> {
        const OPEN_RESOURCE_DEADLINE: Duration = Duration::from_secs(15);

        if resource_path.trim().is_empty() {
            return Err(self.correlate_failure_details(
                "open_resource",
                "resource-path-required",
                failure(WorkbenchFailureCode::Protocol),
                json!({}),
            ));
        }
        let started = Instant::now();
        let value = self
            .gateway
            .request(
                json!({"APIFunc": "RST_WorkbenchOpenResource", "resourcePath": resource_path}),
                OPEN_RESOURCE_DEADLINE,
            )
            .map_err(|failure| {
                self.correlate_failure_details(
                    "open_resource",
                    failure_code(failure.code),
                    failure,
                    json!({"handler": "RST_WorkbenchOpenResource"}),
                )
            })?;
        let raw: RawWorkbenchOpenResource = serde_json::from_value(value).map_err(|_| {
            self.correlate_failure_details(
                "open_resource",
                "workbench_protocol_error",
                failure(WorkbenchFailureCode::Protocol),
                json!({"handler": "RST_WorkbenchOpenResource"}),
            )
        })?;
        let result = WorkbenchOpenResourceResult {
            resource_path: raw.resource_path,
            opened: workbench_bool(&raw.opened),
            status: raw.status,
        };
        self.log_event_timed(
            "open-resource",
            &result.status,
            started,
            json!({"resourcePath": resource_path, "opened": result.opened}),
        );
        Ok(result)
    }

    pub fn project_context(&self) -> Result<WorkbenchProjectContext, WorkbenchFailure> {
        let started = Instant::now();
        let value = self
            .gateway
            .request(
                json!({"APIFunc": "RST_WorkbenchProjectContext"}),
                self.options.gateway.status_deadline,
            )
            .map_err(|failure| {
                self.correlate_failure_details(
                    "project_context",
                    failure_code(failure.code),
                    failure,
                    json!({"handler": "RST_WorkbenchProjectContext"}),
                )
            })?;
        let raw: RawBridgeProjectContext = serde_json::from_value(value).map_err(|_| {
            self.correlate_failure_details(
                "project_context",
                "workbench_protocol_error",
                failure(WorkbenchFailureCode::Protocol),
                json!({"handler": "RST_WorkbenchProjectContext"}),
            )
        })?;
        if raw.bridge_version != WORKBENCH_BRIDGE_VERSION
            || raw.protocol_version != WORKBENCH_BRIDGE_PROTOCOL_VERSION
        {
            return Err(self.correlate_failure_details(
                "project_context", "incompatible-handler", failure(WorkbenchFailureCode::Protocol),
                json!({"handler": "RST_WorkbenchProjectContext", "activeBridgeVersion": raw.bridge_version, "activeProtocolVersion": raw.protocol_version}),
            ));
        }
        let (loaded_addons, loaded_addons_truncated) =
            split_bounded_list(&raw.loaded_addons, 256, 256);
        let result = WorkbenchProjectContext {
            bridge_version: raw.bridge_version,
            protocol_version: raw.protocol_version,
            loaded_addons,
            loaded_addons_truncated,
        };
        self.log_event_timed("project-context", "success", started, json!({"loadedAddonCount": result.loaded_addons.len(), "loadedAddonsTruncated": result.loaded_addons_truncated}));
        Ok(result)
    }

    pub fn loaded_addon_graph(&self) -> Result<WorkbenchLoadedAddonGraph, WorkbenchFailure> {
        self.loaded_addon_graph_with_timing()
            .map(|(graph, _)| graph)
    }

    pub fn loaded_addon_graph_with_timing(
        &self,
    ) -> Result<(WorkbenchLoadedAddonGraph, WorkbenchRequestTiming), WorkbenchFailure> {
        let started = Instant::now();
        let (value, timing) = self
            .gateway
            .request_with_timing(
                json!({"APIFunc": "RST_WorkbenchLoadedAddonGraph"}),
                self.options.gateway.status_deadline,
            )
            .map_err(|failure| {
                self.correlate_failure_details(
                    "loaded_addon_graph",
                    failure_code(failure.code),
                    failure,
                    json!({"handler": "RST_WorkbenchLoadedAddonGraph"}),
                )
            })?;
        let raw: RawWorkbenchLoadedAddonGraph = serde_json::from_value(value).map_err(|_| {
            self.correlate_failure_details(
                "loaded_addon_graph",
                "workbench_protocol_error",
                failure(WorkbenchFailureCode::Protocol),
                json!({"handler": "RST_WorkbenchLoadedAddonGraph"}),
            )
        })?;
        if raw.protocol_version != WORKBENCH_BRIDGE_PROTOCOL_VERSION {
            return Err(self.correlate_failure_details(
                "loaded_addon_graph",
                "incompatible-handler",
                failure(WorkbenchFailureCode::Protocol),
                json!({"handler": "RST_WorkbenchLoadedAddonGraph", "activeBridgeVersion": raw.bridge_version, "activeProtocolVersion": raw.protocol_version}),
            ));
        }
        let mut addons: Vec<WorkbenchLoadedAddon> =
            serde_json::from_str(&raw.graph_json).map_err(|_| {
                self.correlate_failure_details(
                    "loaded_addon_graph",
                    "workbench_protocol_error",
                    failure(WorkbenchFailureCode::Protocol),
                    json!({"handler": "RST_WorkbenchLoadedAddonGraph"}),
                )
            })?;
        let host = &self.host;
        let current_project_file = match raw.current_project_file.as_str() {
            "" => None,
            value => Some(host.to_host_path(value).ok_or_else(|| {
                self.correlate_failure_details(
                    "loaded_addon_graph",
                    "unresolved-workbench-path",
                    failure(WorkbenchFailureCode::Protocol),
                    json!({
                        "handler": "RST_WorkbenchLoadedAddonGraph",
                        "workbenchHost": host.source(),
                    }),
                )
            })?),
        };
        for addon in &mut addons {
            if addon.source_root.as_os_str().is_empty() {
                continue;
            }
            addon.source_root = addon
                .source_root
                .to_str()
                .and_then(|value| host.to_host_path(value))
                .ok_or_else(|| {
                    self.correlate_failure_details(
                        "loaded_addon_graph",
                        "unresolved-workbench-path",
                        failure(WorkbenchFailureCode::Protocol),
                        json!({
                            "handler": "RST_WorkbenchLoadedAddonGraph",
                            "workbenchHost": host.source(),
                        }),
                    )
                })?;
        }
        resolve_loaded_addon_roots(
            host,
            &mut addons,
            current_project_file.as_deref(),
            &self.paths().profile,
        )
        .map_err(|_| {
            self.correlate_failure_details(
                "loaded_addon_graph",
                "unresolved-loaded-addon-root",
                failure(WorkbenchFailureCode::Protocol),
                json!({"handler": "RST_WorkbenchLoadedAddonGraph"}),
            )
        })?;
        if addons.len() > 256
            || addons
                .iter()
                .any(|addon| !valid_loaded_addon(addon) || !addon.source_root.is_absolute())
        {
            return Err(self.correlate_failure_details(
                "loaded_addon_graph",
                "workbench_protocol_error",
                failure(WorkbenchFailureCode::Protocol),
                json!({"handler": "RST_WorkbenchLoadedAddonGraph"}),
            ));
        }
        let mut loaded_instances = HashSet::new();
        if addons
            .iter()
            .any(|addon| !loaded_instances.insert((addon.guid.clone(), addon.source_root.clone())))
        {
            return Err(self.correlate_failure_details(
                "loaded_addon_graph",
                "workbench_protocol_error",
                failure(WorkbenchFailureCode::Protocol),
                json!({"handler": "RST_WorkbenchLoadedAddonGraph"}),
            ));
        }
        let graph = WorkbenchLoadedAddonGraph {
            bridge_version: raw.bridge_version,
            protocol_version: raw.protocol_version,
            current_project_file,
            addons,
        };
        self.log_event_timed(
            "loaded-addon-graph",
            "success",
            started,
            json!({"loadedAddonCount": graph.addons.len()}),
        );
        Ok((graph, timing))
    }

    pub fn inspect_resource(
        &self,
        resource_name: &str,
    ) -> Result<WorkbenchResourceInspection, WorkbenchFailure> {
        let started = Instant::now();
        if !canonical_resource_name(resource_name) {
            return Err(self.correlate_failure_details(
                "inspect_resource",
                "invalid-resource-name",
                failure(WorkbenchFailureCode::Protocol),
                json!({}),
            ));
        }
        let value = self
            .gateway
            .request(
                json!({"APIFunc": "RST_WorkbenchInspectResource", "resourceName": resource_name}),
                self.options.gateway.status_deadline,
            )
            .map_err(|failure| {
                self.correlate_failure_details(
                    "inspect_resource",
                    failure_code(failure.code),
                    failure,
                    json!({"handler": "RST_WorkbenchInspectResource"}),
                )
            })?;
        let raw: RawBridgeResourceInspection = serde_json::from_value(value).map_err(|_| {
            self.correlate_failure_details(
                "inspect_resource",
                "workbench_protocol_error",
                failure(WorkbenchFailureCode::Protocol),
                json!({"handler": "RST_WorkbenchInspectResource"}),
            )
        })?;
        let (source_addons, source_addons_truncated) =
            split_bounded_list(&raw.source_addons, 64, 4 * 1024);
        let result = WorkbenchResourceInspection {
            found: workbench_bool(&raw.found),
            status: raw.status,
            resource_name: raw.resource_name,
            class_name: raw.class_name,
            source_addons,
            source_addons_truncated: workbench_bool(&raw.source_addons_truncated)
                || source_addons_truncated,
        };
        self.log_event_timed(
            "inspect-resource",
            &result.status,
            started,
            json!({"found": result.found}),
        );
        Ok(result)
    }

    pub fn search_resources(
        &self,
        kinds: &[&str],
        query: Option<&str>,
        root_path: Option<&str>,
        addon_guid: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<WorkbenchResourceSearchPage, WorkbenchFailure> {
        let limit = limit.clamp(1, 200);
        let query = query.unwrap_or("").trim();
        let root_path = root_path.unwrap_or("").trim();
        let addon_guid = addon_guid.unwrap_or("").trim();
        if !valid_resource_root_path(root_path) {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        if !addon_guid.is_empty()
            && (addon_guid.len() != 16 || !addon_guid.bytes().all(|byte| byte.is_ascii_hexdigit()))
        {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        let kinds = kinds.join(";");
        let signature = sha256(format!("{kinds}\n{query}\n{root_path}\n{addon_guid}").as_bytes());
        let offset = if let Some(cursor) = cursor {
            let (cursor_signature, offset) = parse_resource_list_cursor(cursor)
                .ok_or_else(|| failure(WorkbenchFailureCode::Protocol))?;
            if cursor_signature != signature {
                return Err(failure(WorkbenchFailureCode::Protocol));
            }
            offset
        } else {
            0
        };
        let started = Instant::now();
        let value = self.gateway.request(
            json!({"APIFunc": "RST_WorkbenchListResources", "extensions": kinds, "query": query, "rootPath": root_path, "addonGuid": addon_guid, "offset": offset, "limit": limit}),
            self.options.gateway.status_deadline,
        ).map_err(|failure| self.correlate_failure_details(
            "search_resources", failure_code(failure.code), failure, json!({"handler": "RST_WorkbenchListResources"}),
        ))?;
        let raw: RawBridgeResourceList = serde_json::from_value(value).map_err(|_| {
            self.correlate_failure_details(
                "search_resources",
                "workbench_protocol_error",
                failure(WorkbenchFailureCode::Protocol),
                json!({"handler": "RST_WorkbenchListResources"}),
            )
        })?;
        let resources = split_bounded_list(&raw.resources, limit, 256 * 1024).0;
        let resource_details = split_bounded_list(&raw.resource_details, limit, 256 * 1024).0;
        if raw.protocol_version != WORKBENCH_BRIDGE_PROTOCOL_VERSION
            || resources.len() > limit
            || resource_details.len() != resources.len()
        {
            return Err(self.correlate_failure_details(
                "search_resources",
                "workbench_protocol_error",
                failure(WorkbenchFailureCode::Protocol),
                json!({"handler": "RST_WorkbenchListResources"}),
            ));
        }
        let project_revision = sha256(raw.loaded_addons.as_bytes());
        let has_more = workbench_bool(&raw.has_more);
        let next_cursor =
            has_more.then(|| format!("wrl1:{signature}:{}", offset + resources.len()));
        let results = resource_details
            .iter()
            .map(|resource_name| parse_resource_search_hit(resource_name))
            .collect::<Result<Vec<_>, _>>()?;
        let result = WorkbenchResourceSearchPage {
            bridge_version: raw.bridge_version,
            protocol_version: raw.protocol_version,
            project_revision,
            limit,
            results,
            truncated: has_more,
            next_cursor,
        };
        self.log_event_timed(
            "search-resources",
            "success",
            started,
            json!({"returned": result.results.len(), "hasMore": result.next_cursor.is_some()}),
        );
        Ok(result)
    }

    pub fn world_selection_summary(
        &self,
    ) -> Result<WorkbenchWorldSelectionSummary, WorkbenchFailure> {
        let started = Instant::now();
        let value = self
            .gateway
            .request(
                json!({"APIFunc": "RST_WorkbenchWorldSelection"}),
                self.options.gateway.status_deadline,
            )
            .map_err(|failure| {
                self.correlate_failure_details(
                    "world_selection_summary",
                    failure_code(failure.code),
                    failure,
                    json!({"handler": "RST_WorkbenchWorldSelection"}),
                )
            })?;
        let raw: RawBridgeWorldSelection = serde_json::from_value(value).map_err(|_| {
            self.correlate_failure_details(
                "world_selection_summary",
                "workbench_protocol_error",
                failure(WorkbenchFailureCode::Protocol),
                json!({"handler": "RST_WorkbenchWorldSelection"}),
            )
        })?;
        if raw.protocol_version != WORKBENCH_BRIDGE_PROTOCOL_VERSION {
            return Err(self.correlate_failure_details(
                "world_selection_summary",
                "incompatible-handler",
                failure(WorkbenchFailureCode::Protocol),
                json!({"handler": "RST_WorkbenchWorldSelection", "activeBridgeVersion": raw.bridge_version, "activeProtocolVersion": raw.protocol_version}),
            ));
        }
        let selected_entities =
            parse_world_selection_records(&raw.selected_entities).map_err(|_| {
                self.correlate_failure_details(
                    "world_selection_summary",
                    "workbench_protocol_error",
                    failure(WorkbenchFailureCode::Protocol),
                    json!({"handler": "RST_WorkbenchWorldSelection"}),
                )
            })?;
        if selected_entities.len() > 32 || selected_entities.len() > raw.selected_count as usize {
            return Err(self.correlate_failure_details(
                "world_selection_summary",
                "workbench_protocol_error",
                failure(WorkbenchFailureCode::Protocol),
                json!({"handler": "RST_WorkbenchWorldSelection"}),
            ));
        }
        let result = WorkbenchWorldSelectionSummary {
            bridge_version: raw.bridge_version,
            protocol_version: raw.protocol_version,
            editor_available: workbench_bool(&raw.editor_available),
            status: raw.status,
            selected_count: raw.selected_count,
            selected_entities_truncated: workbench_bool(&raw.selected_entities_truncated)
                || selected_entities.len() < raw.selected_count as usize,
            selected_entities,
        };
        self.log_event_timed(
            "world-selection-summary",
            &result.status,
            started,
            json!({"editorAvailable": result.editor_available, "selectedCount": result.selected_count, "selectedEntitiesReturned": result.selected_entities.len(), "selectedEntitiesTruncated": result.selected_entities_truncated}),
        );
        Ok(result)
    }

    pub fn set_play_session(
        &self,
        start: bool,
        debug_mode: bool,
        full_screen: bool,
    ) -> Result<WorkbenchPlaySessionResult, WorkbenchFailure> {
        let started = Instant::now();
        let operation = if start {
            "start_play_session"
        } else {
            "stop_play_session"
        };
        let value = self.gateway.request(
            json!({"APIFunc": "RST_WorkbenchPlaySession", "start": start, "debugMode": debug_mode, "fullScreen": full_screen}),
            self.options.gateway.status_deadline,
        ).map_err(|failure| self.correlate_failure_details(
            operation, failure_code(failure.code), failure, json!({"handler": "RST_WorkbenchPlaySession"}),
        ))?;
        let raw: RawBridgePlaySessionResult = serde_json::from_value(value).map_err(|_| {
            self.correlate_failure_details(
                operation,
                "workbench_protocol_error",
                failure(WorkbenchFailureCode::Protocol),
                json!({"handler": "RST_WorkbenchPlaySession"}),
            )
        })?;
        let result = WorkbenchPlaySessionResult {
            accepted: workbench_bool(&raw.accepted),
            status: raw.status,
        };
        self.log_event_timed(
            operation,
            &result.status,
            started,
            json!({"accepted": result.accepted}),
        );
        Ok(result)
    }

    pub fn selected_entity_hierarchy(
        &self,
        selection_index: u32,
    ) -> Result<WorkbenchSelectedEntityHierarchy, WorkbenchFailure> {
        if selection_index > 31 {
            return Err(self.correlate_failure_details(
                "selected_entity_hierarchy",
                "selection-index-out-of-range",
                failure(WorkbenchFailureCode::Protocol),
                json!({"selectionIndex": selection_index}),
            ));
        }
        let started = Instant::now();
        let value = self.gateway.request(
            json!({"APIFunc": "RST_WorkbenchSelectedEntityHierarchy", "selectionIndex": selection_index}),
            self.options.gateway.status_deadline,
        ).map_err(|failure| self.correlate_failure_details(
            "selected_entity_hierarchy", failure_code(failure.code), failure,
            json!({"handler": "RST_WorkbenchSelectedEntityHierarchy"}),
        ))?;
        let raw: RawBridgeSelectedEntityHierarchy =
            serde_json::from_value(value).map_err(|_| {
                self.correlate_failure_details(
                    "selected_entity_hierarchy",
                    "workbench_protocol_error",
                    failure(WorkbenchFailureCode::Protocol),
                    json!({"handler": "RST_WorkbenchSelectedEntityHierarchy"}),
                )
            })?;
        if raw.protocol_version != WORKBENCH_BRIDGE_PROTOCOL_VERSION {
            return Err(self.correlate_failure_details(
                "selected_entity_hierarchy", "incompatible-handler", failure(WorkbenchFailureCode::Protocol),
                json!({"handler": "RST_WorkbenchSelectedEntityHierarchy", "activeBridgeVersion": raw.bridge_version, "activeProtocolVersion": raw.protocol_version}),
            ));
        }
        let parse_error = || {
            self.correlate_failure_details(
                "selected_entity_hierarchy",
                "workbench_protocol_error",
                failure(WorkbenchFailureCode::Protocol),
                json!({"handler": "RST_WorkbenchSelectedEntityHierarchy"}),
            )
        };
        let entity =
            parse_optional_world_selection_record(&raw.entity).map_err(|_| parse_error())?;
        let ancestors = parse_world_selection_records(&raw.ancestors).map_err(|_| parse_error())?;
        let children = parse_world_selection_records(&raw.children).map_err(|_| parse_error())?;
        if ancestors.len() > 32 || children.len() > 64 {
            return Err(self.correlate_failure_details(
                "selected_entity_hierarchy",
                "workbench_protocol_error",
                failure(WorkbenchFailureCode::Protocol),
                json!({"handler": "RST_WorkbenchSelectedEntityHierarchy"}),
            ));
        }
        let result = WorkbenchSelectedEntityHierarchy {
            bridge_version: raw.bridge_version,
            protocol_version: raw.protocol_version,
            editor_available: workbench_bool(&raw.editor_available),
            status: raw.status,
            selection_index,
            entity,
            ancestors,
            ancestors_truncated: workbench_bool(&raw.ancestors_truncated),
            children,
            children_truncated: workbench_bool(&raw.children_truncated),
        };
        self.log_event_timed(
            "selected-entity-hierarchy", &result.status, started,
            json!({"selectionIndex": selection_index, "ancestorCount": result.ancestors.len(), "ancestorsTruncated": result.ancestors_truncated, "childCount": result.children.len(), "childrenTruncated": result.children_truncated}),
        );
        Ok(result)
    }

    pub fn list_entities(
        &self,
        query: Option<&str>,
        class_name: Option<&str>,
        sub_scene: Option<i32>,
        layer_id: Option<i32>,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<WorkbenchEntityListPage, WorkbenchFailure> {
        let signature = sha256(
            format!(
                "{}\n{}\n{}\n{}",
                query.unwrap_or_default(),
                class_name.unwrap_or_default(),
                sub_scene.map_or(String::new(), |value| value.to_string()),
                layer_id.map_or(String::new(), |value| value.to_string())
            )
            .as_bytes(),
        );
        let offset = match cursor {
            Some(cursor) => {
                let (found, offset) = parse_entity_list_cursor(cursor)
                    .ok_or_else(|| failure(WorkbenchFailureCode::Protocol))?;
                (found == signature)
                    .then_some(offset)
                    .ok_or_else(|| failure(WorkbenchFailureCode::Protocol))?
            }
            None => 0,
        };
        let value = self.gateway.request(json!({"APIFunc":"RST_WorkbenchListEntities","query":query.unwrap_or_default(),"className":class_name.unwrap_or_default(),"subScene":sub_scene.unwrap_or(-1),"layerId":layer_id.unwrap_or(-1),"offset":offset,"limit":limit}), self.options.gateway.status_deadline)?;
        let raw: RawBridgeEntityList =
            serde_json::from_value(value).map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        let entities = parse_world_selection_records(&raw.entities)
            .map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        if raw.bridge_version != WORKBENCH_BRIDGE_VERSION
            || raw.protocol_version != WORKBENCH_BRIDGE_PROTOCOL_VERSION
            || entities.len() > limit
        {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        Ok(WorkbenchEntityListPage {
            bridge_version: raw.bridge_version,
            protocol_version: raw.protocol_version,
            world_revision: sha256(raw.world_path.as_bytes()),
            limit,
            next_cursor: workbench_bool(&raw.has_more)
                .then(|| format!("wel1:{signature}:{}", offset + entities.len())),
            truncated: workbench_bool(&raw.has_more),
            entities,
        })
    }

    pub fn search_entities(
        &self,
        query: Option<&str>,
        class_name: Option<&str>,
        resource_query: Option<&str>,
        component_classes: &[&str],
        relation: Option<&WorkbenchEntityRelationFilter>,
        sub_scene: Option<i32>,
        layer_id: Option<i32>,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<WorkbenchEntitySearchPage, WorkbenchFailure> {
        let limit = limit.clamp(1, 100);
        if component_classes
            .iter()
            .any(|class_name| !valid_component_class_name(class_name))
        {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        if let Some(relation) = relation {
            if relation
                .class_name
                .as_deref()
                .is_some_and(|class_name| !valid_component_class_name(class_name))
                || relation.component_classes.len() > 32
                || relation
                    .component_classes
                    .iter()
                    .any(|class_name| !valid_component_class_name(class_name))
                || (relation.class_name.is_none() && relation.component_classes.is_empty())
                || relation.max_depth == 0
                || relation.max_depth > 8
                || matches!(
                    relation.direction,
                    WorkbenchEntityRelationDirection::Parent
                        | WorkbenchEntityRelationDirection::Child
                ) && relation.max_depth != 1
            {
                return Err(failure(WorkbenchFailureCode::Protocol));
            }
        }
        let components = component_classes.join(";");
        let (
            relation_direction,
            relation_class_name,
            relation_component_classes,
            relation_max_depth,
        ) = relation
            .map(|relation| {
                (
                    match relation.direction {
                        WorkbenchEntityRelationDirection::Parent => "parent",
                        WorkbenchEntityRelationDirection::Ancestor => "ancestor",
                        WorkbenchEntityRelationDirection::Child => "child",
                        WorkbenchEntityRelationDirection::Descendant => "descendant",
                    },
                    relation.class_name.as_deref().unwrap_or_default(),
                    relation.component_classes.join(";"),
                    relation.max_depth,
                )
            })
            .unwrap_or(("", "", String::new(), 0));
        let signature = sha256(
            format!(
                "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
                query.unwrap_or_default(),
                class_name.unwrap_or_default(),
                resource_query.unwrap_or_default(),
                components,
                relation_direction,
                relation_class_name,
                relation_component_classes,
                relation_max_depth,
                sub_scene.map_or(String::new(), |v| v.to_string()),
                layer_id.map_or(String::new(), |v| v.to_string())
            )
            .as_bytes(),
        );
        let offset = match cursor {
            Some(cursor) => {
                let (found, offset) = parse_entity_list_cursor(cursor)
                    .ok_or_else(|| failure(WorkbenchFailureCode::Protocol))?;
                (found == signature)
                    .then_some(offset)
                    .ok_or_else(|| failure(WorkbenchFailureCode::Protocol))?
            }
            None => 0,
        };
        let value = self.gateway.request(json!({"APIFunc":"RST_WorkbenchSearchEntities","query":query.unwrap_or_default(),"className":class_name.unwrap_or_default(),"resourceQuery":resource_query.unwrap_or_default(),"componentClasses":components,"relationDirection":relation_direction,"relationClassName":relation_class_name,"relationComponentClasses":relation_component_classes,"relationMaxDepth":relation_max_depth,"subScene":sub_scene.unwrap_or(-1),"layerId":layer_id.unwrap_or(-1),"offset":offset,"limit":limit}), self.options.gateway.status_deadline)?;
        let raw: RawBridgeEntitySearch =
            serde_json::from_value(value).map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        let results = parse_entity_search_records(&raw.results)
            .map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        let available = raw.status == "available";
        if raw.bridge_version != WORKBENCH_BRIDGE_VERSION
            || raw.protocol_version != WORKBENCH_BRIDGE_PROTOCOL_VERSION
            || results.len() > limit
            || (available
                && (raw.named_matches > raw.total_matches
                    || raw.total_matches < results.len() as u32))
            || (!available
                && (!results.is_empty()
                    || raw.total_matches != 0
                    || raw.named_matches != 0
                    || workbench_bool(&raw.has_more)))
        {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        Ok(WorkbenchEntitySearchPage {
            bridge_version: raw.bridge_version,
            protocol_version: raw.protocol_version,
            world_revision: sha256(raw.world_path.as_bytes()),
            status: raw.status,
            limit,
            summary: WorkbenchEntitySearchSummary {
                total_matches: raw.total_matches,
                named_matches: raw.named_matches,
                anonymous_matches: raw.total_matches.saturating_sub(raw.named_matches),
            },
            next_cursor: (available && workbench_bool(&raw.has_more))
                .then(|| format!("wel1:{signature}:{}", offset + results.len())),
            truncated: available && workbench_bool(&raw.has_more),
            relation_traversal_truncated: workbench_bool(&raw.relation_traversal_truncated),
            results,
        })
    }

    pub fn layer_state(
        &self,
        sub_scene: i32,
        layer_id: i32,
    ) -> Result<WorkbenchLayerState, WorkbenchFailure> {
        let value = self.gateway.request(
            json!({"APIFunc":"RST_WorkbenchLayerState","subScene":sub_scene,"layerId":layer_id}),
            self.options.gateway.status_deadline,
        )?;
        let raw: RawBridgeLayerState =
            serde_json::from_value(value).map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        if raw.bridge_version != WORKBENCH_BRIDGE_VERSION
            || raw.protocol_version != WORKBENCH_BRIDGE_PROTOCOL_VERSION
            || raw.sub_scene != sub_scene
            || raw.layer_id != layer_id
        {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        Ok(WorkbenchLayerState {
            bridge_version: raw.bridge_version,
            protocol_version: raw.protocol_version,
            status: raw.status,
            sub_scene: raw.sub_scene,
            layer_id: raw.layer_id,
            layer_path: raw.layer_path,
            visible: workbench_bool(&raw.visible),
            explicitly_locked: workbench_bool(&raw.explicitly_locked),
            locked_in_hierarchy: workbench_bool(&raw.locked_in_hierarchy),
        })
    }

    pub fn inspect_entity(
        &self,
        entity_id: &str,
    ) -> Result<WorkbenchEntityInspection, WorkbenchFailure> {
        let value = self.gateway.request(
            json!({"APIFunc":"RST_WorkbenchInspectEntity","entityId":entity_id}),
            self.options.gateway.status_deadline,
        )?;
        let raw: RawBridgeSelectedEntityHierarchy =
            serde_json::from_value(value).map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        if raw.protocol_version != WORKBENCH_BRIDGE_PROTOCOL_VERSION {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        let entity = parse_optional_world_selection_record(&raw.entity)
            .map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        let ancestors = parse_world_selection_records(&raw.ancestors)
            .map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        let children = parse_world_selection_records(&raw.children)
            .map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        if ancestors.len() > 32 || children.len() > 64 {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        let components = parse_component_summaries(&raw.components, &raw.component_properties)
            .map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        let mut properties = parse_prefab_properties(&raw.properties)
            .map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        properties.retain(|property| {
            property.directly_overridden
                || matches!(property.path.as_str(), "coords" | "angles" | "scale")
        });
        let (contributor_addons, contributor_addons_truncated) =
            split_bounded_list(&raw.contributor_addons, 64, 4 * 1024);
        Ok(WorkbenchEntityInspection {
            bridge_version: raw.bridge_version,
            protocol_version: raw.protocol_version,
            editor_available: workbench_bool(&raw.editor_available),
            status: raw.status,
            entity,
            resource_name: raw.resource_name,
            resource_reference_kind: raw.resource_reference_kind,
            contributor_addons,
            contributor_addons_truncated: workbench_bool(&raw.contributor_addons_truncated)
                || contributor_addons_truncated,
            ancestors,
            ancestors_truncated: workbench_bool(&raw.ancestors_truncated),
            children,
            children_truncated: workbench_bool(&raw.children_truncated),
            components,
            component_properties_truncated: workbench_bool(&raw.component_properties_truncated),
            properties,
            properties_truncated: workbench_bool(&raw.properties_truncated),
        })
    }

    pub fn find_entities_by_radius(
        &self,
        options: WorkbenchEntityRadiusQueryOptions,
    ) -> Result<WorkbenchEntityRadiusQuery, WorkbenchFailure> {
        if !(0.01..=50_000.0).contains(&options.radius_meters)
            || !(1..=100).contains(&options.limit)
            || !matches!(options.query_scope.as_str(), "all" | "static" | "dynamic")
        {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        let value = self.gateway.request(
            json!({"APIFunc":"RST_WorkbenchFindEntitiesByRadius","centerX":options.center.x,"centerY":options.center.y,"centerZ":options.center.z,"radiusMeters":options.radius_meters,"queryScope":options.query_scope,"requireObject":options.require_object,"excludeProxies":options.exclude_proxies,"className":options.class_name.unwrap_or_default(),"limit":options.limit}),
            self.options.gateway.status_deadline,
        )?;
        let raw: RawBridgeEntityRadiusQuery =
            serde_json::from_value(value).map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        let entities = parse_world_selection_records(&raw.entities)
            .map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        if raw.protocol_version != WORKBENCH_BRIDGE_PROTOCOL_VERSION
            || entities.len() > options.limit
        {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        Ok(WorkbenchEntityRadiusQuery {
            bridge_version: raw.bridge_version,
            protocol_version: raw.protocol_version,
            status: raw.status,
            center: WorkbenchEntityPosition {
                x: raw.center_x,
                y: raw.center_y,
                z: raw.center_z,
            },
            radius_meters: raw.radius_meters,
            query_scope: raw.query_scope,
            require_object: workbench_bool(&raw.require_object),
            exclude_proxies: workbench_bool(&raw.exclude_proxies),
            entities,
            truncated: workbench_bool(&raw.truncated),
        })
    }

    pub fn sample_terrain(
        &self,
        options: WorkbenchTerrainSampleOptions,
    ) -> Result<WorkbenchTerrainSample, WorkbenchFailure> {
        const MAX_HALF_EXTENT_METERS: f32 = 500.0;
        const MAX_SPACING_METERS: f32 = 500.0;
        const MAX_SAMPLES: usize = 4_096;
        if !options.center_x.is_finite()
            || !options.center_z.is_finite()
            || !(0.01..=MAX_HALF_EXTENT_METERS).contains(&options.half_extent_meters)
            || options.spacing_meters.is_some_and(|spacing| {
                !spacing.is_finite() || !(0.01..=MAX_SPACING_METERS).contains(&spacing)
            })
        {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        let requested_spacing = options.spacing_meters.unwrap_or(0.0);
        let value = self.gateway.request(
            json!({"APIFunc":"RST_WorkbenchSampleTerrain","centerX":options.center_x,"centerZ":options.center_z,"halfExtentMeters":options.half_extent_meters,"spacingMeters":requested_spacing,"includeWater":options.include_water}),
            self.options.gateway.status_deadline,
        )?;
        let raw: RawBridgeTerrainSample =
            serde_json::from_value(value).map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        if raw.bridge_version != WORKBENCH_BRIDGE_VERSION
            || raw.protocol_version != WORKBENCH_BRIDGE_PROTOCOL_VERSION
        {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        if raw.status != "available" {
            return Ok(WorkbenchTerrainSample {
                bridge_version: raw.bridge_version,
                protocol_version: raw.protocol_version,
                status: raw.status,
                terrain: None,
                grid: None,
                summary: None,
                water: None,
                water_summary: None,
            });
        }
        let terrain =
            parse_terrain_metadata(&raw).map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        let grid = parse_terrain_grid(&raw, options.spacing_meters, MAX_SAMPLES)
            .map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        let summary = summarize_terrain_grid(&grid);
        let water = options
            .include_water
            .then(|| parse_terrain_water_grid(&raw, grid.heights.len()))
            .transpose()
            .map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        let water_summary = water.as_ref().map(summarize_terrain_water_grid);
        Ok(WorkbenchTerrainSample {
            bridge_version: raw.bridge_version,
            protocol_version: raw.protocol_version,
            status: raw.status,
            terrain: Some(terrain),
            grid: Some(grid),
            summary: Some(summary),
            water,
            water_summary,
        })
    }

    pub fn viewport_context(
        &self,
        options: WorkbenchViewportContextOptions,
    ) -> Result<WorkbenchViewportContext, WorkbenchFailure> {
        let value = self.gateway.request(
            json!({"APIFunc":"RST_WorkbenchViewportContext"}),
            self.options.gateway.status_deadline,
        )?;
        let raw: RawBridgeViewportContext =
            serde_json::from_value(value).map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        if raw.bridge_version != WORKBENCH_BRIDGE_VERSION
            || raw.protocol_version != WORKBENCH_BRIDGE_PROTOCOL_VERSION
        {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        if raw.status != "available" {
            return Ok(WorkbenchViewportContext {
                bridge_version: raw.bridge_version,
                protocol_version: raw.protocol_version,
                status: raw.status,
                width: options.include_ray.then_some(raw.width),
                height: options.include_ray.then_some(raw.height),
                mouse_x: options.include_ray.then_some(raw.mouse_x),
                mouse_y: options.include_ray.then_some(raw.mouse_y),
                mouse_inside: workbench_bool(&raw.mouse_inside),
                camera_position: None,
                camera_direction: None,
                mouse_world_position: None,
                ray_start: None,
                ray_end: None,
                ray_direction: None,
            });
        }
        let position =
            |x: f32, y: f32, z: f32| {
                (x.is_finite() && y.is_finite() && z.is_finite())
                    .then_some(WorkbenchEntityPosition { x, y, z })
            };
        let ray_start = position(raw.start_x, raw.start_y, raw.start_z);
        Ok(WorkbenchViewportContext {
            bridge_version: raw.bridge_version,
            protocol_version: raw.protocol_version,
            status: raw.status,
            width: options.include_ray.then_some(raw.width),
            height: options.include_ray.then_some(raw.height),
            mouse_x: options.include_ray.then_some(raw.mouse_x),
            mouse_y: options.include_ray.then_some(raw.mouse_y),
            mouse_inside: workbench_bool(&raw.mouse_inside),
            camera_position: position(raw.camera_x, raw.camera_y, raw.camera_z),
            camera_direction: options
                .include_ray
                .then(|| {
                    position(
                        raw.camera_direction_x,
                        raw.camera_direction_y,
                        raw.camera_direction_z,
                    )
                })
                .flatten(),
            mouse_world_position: position(raw.end_x, raw.end_y, raw.end_z),
            ray_start: options.include_ray.then_some(ray_start).flatten(),
            ray_end: options
                .include_ray
                .then(|| position(raw.end_x, raw.end_y, raw.end_z))
                .flatten(),
            ray_direction: options
                .include_ray
                .then(|| position(raw.direction_x, raw.direction_y, raw.direction_z))
                .flatten(),
        })
    }

    pub fn trace(
        &self,
        options: WorkbenchTraceOptions,
    ) -> Result<WorkbenchTraceResult, WorkbenchFailure> {
        let valid =
            |p: &WorkbenchEntityPosition| p.x.is_finite() && p.y.is_finite() && p.z.is_finite();
        let trace_length = ((options.end.x - options.start.x).powi(2)
            + (options.end.y - options.start.y).powi(2)
            + (options.end.z - options.start.z).powi(2))
        .sqrt();
        if !valid(&options.start)
            || !valid(&options.end)
            || !trace_length.is_finite()
            || !(0.0..=MAX_WORKBENCH_TRACE_LENGTH_METERS).contains(&trace_length)
            || (!options.entities && !options.terrain && !options.ocean)
            || options
                .radius
                .is_some_and(|v| !v.is_finite() || !(0.001..=1000.0).contains(&v))
            || options.box_mins.as_ref().is_some_and(|p| !valid(p))
            || options.box_maxs.as_ref().is_some_and(|p| !valid(p))
            || options.target_layers.is_some_and(|layers| {
                !(1..=MAX_WORKBENCH_TARGET_LAYER_MASK).contains(&layers) || !options.entities
            })
        {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        if let (Some(mins), Some(maxs)) = (&options.box_mins, &options.box_maxs) {
            if mins.x < 0.0
                || mins.y < 0.0
                || mins.z < 0.0
                || maxs.x < mins.x
                || maxs.y < mins.y
                || maxs.z < mins.z
                || maxs.x - mins.x > MAX_WORKBENCH_TRACE_DIMENSION_METERS
                || maxs.y - mins.y > MAX_WORKBENCH_TRACE_DIMENSION_METERS
                || maxs.z - mins.z > MAX_WORKBENCH_TRACE_DIMENSION_METERS
                || (maxs.x == mins.x && maxs.y == mins.y && maxs.z == mins.z)
            {
                return Err(failure(WorkbenchFailureCode::Protocol));
            }
        }
        let shape = match options.shape {
            WorkbenchTraceShape::Line => "line",
            WorkbenchTraceShape::Sphere => "sphere",
            WorkbenchTraceShape::Box => "box",
        };
        if matches!(options.shape, WorkbenchTraceShape::Sphere) && options.radius.is_none()
            || matches!(options.shape, WorkbenchTraceShape::Box)
                && (options.box_mins.is_none() || options.box_maxs.is_none())
        {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        let value = self.gateway.request(json!({"APIFunc":"RST_WorkbenchTrace","startX":options.start.x,"startY":options.start.y,"startZ":options.start.z,"endX":options.end.x,"endY":options.end.y,"endZ":options.end.z,"shape":shape,"radius":options.radius.unwrap_or(0.0),"minsX":options.box_mins.as_ref().map_or(0.0,|p|p.x),"minsY":options.box_mins.as_ref().map_or(0.0,|p|p.y),"minsZ":options.box_mins.as_ref().map_or(0.0,|p|p.z),"maxsX":options.box_maxs.as_ref().map_or(0.0,|p|p.x),"maxsY":options.box_maxs.as_ref().map_or(0.0,|p|p.y),"maxsZ":options.box_maxs.as_ref().map_or(0.0,|p|p.z),"entities":options.entities,"terrain":options.terrain,"ocean":options.ocean,"targetLayers":options.target_layers.unwrap_or(0)}), self.options.gateway.status_deadline)?;
        let raw: RawBridgeTrace =
            serde_json::from_value(value).map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        if raw.bridge_version != WORKBENCH_BRIDGE_VERSION
            || raw.protocol_version != WORKBENCH_BRIDGE_PROTOCOL_VERSION
        {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        let hit = workbench_bool(&raw.hit);
        let position = hit.then_some(WorkbenchEntityPosition {
            x: raw.hit_x,
            y: raw.hit_y,
            z: raw.hit_z,
        });
        let normal = hit.then_some(WorkbenchEntityPosition {
            x: raw.normal_x,
            y: raw.normal_y,
            z: raw.normal_z,
        });
        Ok(WorkbenchTraceResult {
            bridge_version: raw.bridge_version,
            protocol_version: raw.protocol_version,
            status: (!raw.status.is_empty())
                .then_some(raw.status)
                .unwrap_or_else(|| "available".to_string()),
            hit,
            fraction: hit.then_some(raw.fraction),
            distance: hit.then_some(raw.distance),
            position,
            normal,
            kind: match raw.kind.as_str() {
                "entity" => Some(WorkbenchTraceHitKind::Entity),
                "terrain" => Some(WorkbenchTraceHitKind::Terrain),
                "ocean" => Some(WorkbenchTraceHitKind::Ocean),
                _ => None,
            },
            entity: parse_optional_world_selection_record(&raw.entity)
                .map_err(|_| failure(WorkbenchFailureCode::Protocol))?,
            collider_name: (!raw.collider_name.is_empty()).then_some(raw.collider_name),
            material: (!raw.material.is_empty()).then_some(raw.material),
        })
    }

    pub fn clear_selection(&self) -> Result<WorkbenchWorldSelectionSummary, WorkbenchFailure> {
        self.selection_mutation("RST_WorkbenchClearSelection", json!({}))
    }

    pub fn shape_points(&self, entity_id: &str) -> Result<WorkbenchShapePoints, WorkbenchFailure> {
        self.shape_points_request("RST_WorkbenchShapePoints", json!({"entityId": entity_id}))
    }

    pub fn edit_shape_points(
        &self,
        entity_id: &str,
        edit: WorkbenchShapePointEdit,
        index: Option<usize>,
        count: Option<usize>,
        points: &[WorkbenchEntityPosition],
    ) -> Result<WorkbenchShapePoints, WorkbenchFailure> {
        let operation = match edit {
            WorkbenchShapePointEdit::Set => "set",
            WorkbenchShapePointEdit::Insert => "insert",
            WorkbenchShapePointEdit::Delete => "delete",
        };
        let encoded_points = points
            .iter()
            .map(|point| format!("{},{},{}", point.x, point.y, point.z))
            .collect::<Vec<_>>()
            .join(";");
        self.shape_points_request(
            "RST_WorkbenchEditShapePoints",
            json!({
                "entityId": entity_id,
                "operation": operation,
                "index": index.unwrap_or(0),
                "count": count.unwrap_or(1),
                "points": encoded_points,
            }),
        )
    }

    pub fn convert_shape_points(
        &self,
        entity_id: &str,
        from_space: WorkbenchShapePointSpace,
        to_space: WorkbenchShapePointSpace,
        points: &[WorkbenchEntityPosition],
    ) -> Result<WorkbenchShapePointConversion, WorkbenchFailure> {
        self.shape_geometry_request(
            "RST_WorkbenchShapeGeometry",
            json!({
                "entityId": entity_id,
                "operation": "convert",
                "fromSpace": shape_point_space_name(from_space),
                "toSpace": shape_point_space_name(to_space),
                "points": encode_shape_points(points),
            }),
        )
        .map(RawBridgeShapeGeometry::into_conversion)
    }

    pub fn transform_shape_points(
        &self,
        entity_id: &str,
        space: WorkbenchShapePointSpace,
        operation: WorkbenchShapeTransformOperation,
        offset: WorkbenchEntityPosition,
        pivot: WorkbenchEntityPosition,
        degrees: f32,
        scale: WorkbenchEntityPosition,
        mirror_axis: &str,
    ) -> Result<WorkbenchShapePoints, WorkbenchFailure> {
        let raw = self.shape_geometry_request(
            "RST_WorkbenchShapeGeometry",
            json!({
                "entityId": entity_id, "operation": "transform", "space": shape_point_space_name(space),
                "transformOperation": shape_transform_operation_name(operation),
                "offsetX": offset.x, "offsetY": offset.y, "offsetZ": offset.z,
                "pivotX": pivot.x, "pivotY": pivot.y, "pivotZ": pivot.z,
                "degrees": degrees, "scaleX": scale.x, "scaleY": scale.y, "scaleZ": scale.z,
                "mirrorAxis": mirror_axis,
            }),
        )?;
        Ok(raw.into_shape_points())
    }

    pub fn resample_polyline(
        &self,
        entity_id: &str,
        space: WorkbenchShapePointSpace,
        spacing_meters: f32,
    ) -> Result<WorkbenchPolylineResample, WorkbenchFailure> {
        let raw = self.shape_geometry_request(
            "RST_WorkbenchShapeGeometry",
            json!({"entityId": entity_id, "operation": "resample", "space": shape_point_space_name(space), "spacingMeters": spacing_meters}),
        )?;
        Ok(raw.into_resample())
    }

    pub fn inspect_spline(
        &self,
        entity_id: &str,
        space: WorkbenchShapePointSpace,
    ) -> Result<WorkbenchSpline, WorkbenchFailure> {
        self.spline_request(json!({
            "entityId": entity_id,
            "operation": "inspect",
            "space": shape_point_space_name(space),
        }))
    }

    pub fn edit_spline(
        &self,
        entity_id: &str,
        space: WorkbenchShapePointSpace,
        anchors: &[WorkbenchSplineAnchorInput],
        closed: Option<bool>,
    ) -> Result<WorkbenchSpline, WorkbenchFailure> {
        let encoded = anchors
            .iter()
            .enumerate()
            .map(|(index, anchor)| {
                let mode = match anchor.tangent_mode {
                    WorkbenchSplineTangentModeInput::Auto => "auto",
                    WorkbenchSplineTangentModeInput::Explicit => "explicit",
                };
                let in_tangent = anchor
                    .in_tangent
                    .clone()
                    .unwrap_or(WorkbenchEntityPosition {
                        x: 0.0,
                        y: 0.0,
                        z: 0.0,
                    });
                let out_tangent = anchor
                    .out_tangent
                    .clone()
                    .unwrap_or(WorkbenchEntityPosition {
                        x: 0.0,
                        y: 0.0,
                        z: 0.0,
                    });
                format!(
                    "{index},{mode},{},{},{},{},{},{},{},{},{}",
                    anchor.position.x,
                    anchor.position.y,
                    anchor.position.z,
                    in_tangent.x,
                    in_tangent.y,
                    in_tangent.z,
                    out_tangent.x,
                    out_tangent.y,
                    out_tangent.z,
                )
            })
            .collect::<Vec<_>>()
            .join(";");
        self.spline_request(json!({
            "entityId": entity_id,
            "operation": "edit",
            "space": shape_point_space_name(space),
            "anchors": encoded,
            "hasClosed": closed.is_some(),
            "closed": closed.unwrap_or(false),
        }))
    }

    pub fn sample_spline(
        &self,
        entity_id: &str,
        space: WorkbenchShapePointSpace,
        max_samples: usize,
    ) -> Result<WorkbenchSpline, WorkbenchFailure> {
        self.spline_request(json!({
            "entityId": entity_id,
            "operation": "sample",
            "space": shape_point_space_name(space),
            "maxSamples": max_samples,
        }))
    }

    fn spline_request(&self, request: Value) -> Result<WorkbenchSpline, WorkbenchFailure> {
        let mut payload = request
            .as_object()
            .cloned()
            .ok_or_else(|| failure(WorkbenchFailureCode::Protocol))?;
        payload.insert(
            "APIFunc".to_string(),
            Value::String("RST_WorkbenchSpline".to_string()),
        );
        let raw: RawBridgeSpline = serde_json::from_value(
            self.gateway
                .request(Value::Object(payload), self.options.gateway.status_deadline)?,
        )
        .map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        if raw.bridge_version != WORKBENCH_BRIDGE_VERSION
            || raw.protocol_version != WORKBENCH_BRIDGE_PROTOCOL_VERSION
        {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        raw.into_result()
    }

    pub fn set_selection(
        &self,
        entity_id: &str,
    ) -> Result<WorkbenchEntitySelectionResult, WorkbenchFailure> {
        let value = self.gateway.request(
            json!({"APIFunc":"RST_WorkbenchSetSelection","entityId":entity_id}),
            self.options.gateway.status_deadline,
        )?;
        let raw: RawBridgeEntitySelection =
            serde_json::from_value(value).map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        if raw.protocol_version != WORKBENCH_BRIDGE_PROTOCOL_VERSION {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        let entity = parse_optional_world_selection_record(&raw.entity)
            .map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        Ok(WorkbenchEntitySelectionResult {
            bridge_version: raw.bridge_version,
            protocol_version: raw.protocol_version,
            status: raw.status,
            entity,
        })
    }

    fn shape_points_request(
        &self,
        api_func: &str,
        request: Value,
    ) -> Result<WorkbenchShapePoints, WorkbenchFailure> {
        let mut payload = request
            .as_object()
            .cloned()
            .ok_or_else(|| failure(WorkbenchFailureCode::Protocol))?;
        payload.insert("APIFunc".to_string(), Value::String(api_func.to_string()));
        let value = self
            .gateway
            .request(Value::Object(payload), self.options.gateway.status_deadline)?;
        let raw: RawBridgeShapePoints =
            serde_json::from_value(value).map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        if raw.protocol_version != WORKBENCH_BRIDGE_PROTOCOL_VERSION {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        let entity = parse_optional_world_selection_record(&raw.entity)
            .map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        let points = parse_shape_points(&raw.points)
            .ok_or_else(|| failure(WorkbenchFailureCode::Protocol))?;
        Ok(WorkbenchShapePoints {
            bridge_version: raw.bridge_version,
            protocol_version: raw.protocol_version,
            status: raw.status,
            entity,
            shape_class: (!raw.shape_class.is_empty()).then_some(raw.shape_class),
            closed: raw.closed,
            points,
        })
    }

    fn shape_geometry_request(
        &self,
        api_func: &str,
        request: Value,
    ) -> Result<RawBridgeShapeGeometry, WorkbenchFailure> {
        let mut payload = request
            .as_object()
            .cloned()
            .ok_or_else(|| failure(WorkbenchFailureCode::Protocol))?;
        payload.insert("APIFunc".to_string(), Value::String(api_func.to_string()));
        let raw: RawBridgeShapeGeometry = serde_json::from_value(
            self.gateway
                .request(Value::Object(payload), self.options.gateway.status_deadline)?,
        )
        .map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        if raw.bridge_version != WORKBENCH_BRIDGE_VERSION
            || raw.protocol_version != WORKBENCH_BRIDGE_PROTOCOL_VERSION
        {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        raw.validate()
    }

    pub fn create_entity(
        &self,
        options: WorkbenchCreateEntityOptions,
    ) -> Result<WorkbenchEntityMutationResult, WorkbenchFailure> {
        self.entity_mutation("RST_WorkbenchCreateEntity", json!({"resourceName":options.target,"targetIsResource":options.target_is_resource,"subScene":options.sub_scene,"x":options.position.x,"y":options.position.y,"z":options.position.z,"pitch":options.angles.x,"yaw":options.angles.y,"roll":options.angles.z,"layerId":options.layer_id,"name":options.name.unwrap_or_default()}))
    }

    pub fn rename_entity(
        &self,
        entity_id: &str,
        name: &str,
    ) -> Result<WorkbenchEntityMutationResult, WorkbenchFailure> {
        self.entity_mutation(
            "RST_WorkbenchRenameEntity",
            json!({"entityId":entity_id,"name":name}),
        )
    }

    pub fn move_entity(
        &self,
        entity_id: &str,
        position: WorkbenchEntityPosition,
    ) -> Result<WorkbenchEntityMutationResult, WorkbenchFailure> {
        self.entity_mutation(
            "RST_WorkbenchMoveEntity",
            json!({"entityId":entity_id,"x":position.x,"y":position.y,"z":position.z}),
        )
    }

    pub fn rotate_entity(
        &self,
        entity_id: &str,
        angles: WorkbenchEntityPosition,
    ) -> Result<WorkbenchEntityMutationResult, WorkbenchFailure> {
        self.entity_mutation(
            "RST_WorkbenchRotateEntity",
            json!({"entityId":entity_id,"pitch":angles.x,"yaw":angles.y,"roll":angles.z}),
        )
    }

    pub fn transform_entity(
        &self,
        entity_id: &str,
        transform: WorkbenchEntityTransform,
    ) -> Result<WorkbenchEntityTransformResult, WorkbenchFailure> {
        let value = self.gateway.request(
            json!({
                "APIFunc": "RST_WorkbenchTransformEntity",
                "entityId": entity_id,
                "x": transform.position.x,
                "y": transform.position.y,
                "z": transform.position.z,
                "pitch": transform.angles.x,
                "yaw": transform.angles.y,
                "roll": transform.angles.z,
                "scale": transform.scale,
            }),
            self.options.gateway.status_deadline,
        )?;
        let raw: RawBridgeEntityTransform =
            serde_json::from_value(value).map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        if raw.protocol_version != WORKBENCH_BRIDGE_PROTOCOL_VERSION {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        let entity = parse_optional_world_selection_record(&raw.entity)
            .map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        let transform = match (raw.position.as_deref(), raw.angles.as_deref()) {
            (Some(position), Some(angles)) if raw.scale.is_finite() => {
                Some(WorkbenchEntityTransform {
                    position: parse_vector_record(position)
                        .map_err(|_| failure(WorkbenchFailureCode::Protocol))?,
                    angles: parse_vector_record(angles)
                        .map_err(|_| failure(WorkbenchFailureCode::Protocol))?,
                    scale: raw.scale,
                })
            }
            _ => None,
        };
        Ok(WorkbenchEntityTransformResult {
            bridge_version: raw.bridge_version,
            protocol_version: raw.protocol_version,
            status: raw.status,
            entity,
            transform,
        })
    }

    pub fn undo(&self) -> Result<WorkbenchHistoryResult, WorkbenchFailure> {
        self.history_operation("RST_WorkbenchUndo", "undo")
    }

    pub fn redo(&self) -> Result<WorkbenchHistoryResult, WorkbenchFailure> {
        self.history_operation("RST_WorkbenchRedo", "redo")
    }

    fn history_operation(
        &self,
        api_func: &str,
        operation: &str,
    ) -> Result<WorkbenchHistoryResult, WorkbenchFailure> {
        let value = self.gateway.request(
            json!({"APIFunc": api_func}),
            self.options.gateway.status_deadline,
        )?;
        serde_json::from_value(value)
            .map_err(|_| failure(WorkbenchFailureCode::Protocol))
            .map(|mut result: WorkbenchHistoryResult| {
                result.operation = operation.to_string();
                result
            })
    }

    pub fn reparent_entity(
        &self,
        entity_id: &str,
        parent_entity_id: &str,
    ) -> Result<WorkbenchEntityMutationResult, WorkbenchFailure> {
        self.entity_mutation(
            "RST_WorkbenchReparentEntity",
            json!({"entityId":entity_id,"parentEntityId":parent_entity_id}),
        )
    }

    pub fn duplicate_entity(
        &self,
        entity_id: &str,
        position: WorkbenchEntityPosition,
        name: Option<&str>,
    ) -> Result<WorkbenchEntityMutationResult, WorkbenchFailure> {
        self.entity_mutation(
            "RST_WorkbenchDuplicateEntity",
            json!({"entityId":entity_id,"x":position.x,"y":position.y,"z":position.z,"name":name.unwrap_or_default()}),
        )
    }

    pub fn delete_entity(
        &self,
        entity_id: &str,
        confirmation_token: Option<&str>,
    ) -> Result<WorkbenchEntityMutationResult, WorkbenchFailure> {
        if let Some(token) = confirmation_token {
            let valid = self
                .delete_confirmations
                .lock()
                .unwrap()
                .remove(token)
                .is_some_and(|(bound_entity, issued)| {
                    bound_entity == entity_id && issued.elapsed() <= Duration::from_secs(30)
                });
            if !valid {
                return Ok(WorkbenchEntityMutationResult {
                    bridge_version: WORKBENCH_BRIDGE_VERSION.to_string(),
                    protocol_version: WORKBENCH_BRIDGE_PROTOCOL_VERSION,
                    status: "invalid-confirmation".to_string(),
                    active_layer_id: None,
                    entity: None,
                    confirmation_token: None,
                    destination: None,
                    destination_exists: None,
                    resource_name: None,
                    persistence_path: None,
                    template_saved: None,
                    inspection: None,
                });
            }
            return self.entity_mutation(
                "RST_WorkbenchDeleteEntity",
                json!({"entityId":entity_id,"confirm":true}),
            );
        }
        let mut preview = self.entity_mutation(
            "RST_WorkbenchDeleteEntity",
            json!({"entityId":entity_id,"confirm":false}),
        )?;
        if preview.status == "confirmation-required" {
            let sequence = DELETE_CONFIRMATION_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let token = format!(
                "del1:{}",
                sha256(format!("{entity_id}:{sequence}").as_bytes())
            );
            self.delete_confirmations
                .lock()
                .unwrap()
                .insert(token.clone(), (entity_id.to_string(), Instant::now()));
            preview.confirmation_token = Some(token);
        }
        Ok(preview)
    }

    fn entity_mutation(
        &self,
        api_func: &str,
        mut request: Value,
    ) -> Result<WorkbenchEntityMutationResult, WorkbenchFailure> {
        let started = Instant::now();
        request["APIFunc"] = Value::String(api_func.to_string());
        let audit_request = request.clone();
        let value = self
            .gateway
            .request(request, self.options.gateway.status_deadline)?;
        let raw: RawBridgeEntitySelection =
            serde_json::from_value(value).map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        if raw.protocol_version != WORKBENCH_BRIDGE_PROTOCOL_VERSION {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        let result = WorkbenchEntityMutationResult {
            bridge_version: raw.bridge_version,
            protocol_version: raw.protocol_version,
            status: raw.status,
            active_layer_id: raw.active_layer_id,
            entity: parse_optional_world_selection_record(&raw.entity)
                .map_err(|_| failure(WorkbenchFailureCode::Protocol))?,
            confirmation_token: None,
            destination: (!raw.destination.is_empty()).then_some(raw.destination),
            destination_exists: raw.destination_exists,
            resource_name: None,
            persistence_path: None,
            template_saved: None,
            inspection: None,
        };
        self.log_event_timed(
            entity_mutation_operation(api_func),
            &result.status,
            started,
            entity_mutation_audit_details(&audit_request, &result),
        );
        Ok(result)
    }

    pub fn list_components(
        &self,
        entity_id: &str,
    ) -> Result<WorkbenchComponentResult, WorkbenchFailure> {
        self.component_operation(
            "RST_WorkbenchListComponents",
            json!({"entityId":entity_id}),
            None,
        )
    }
    pub fn inspect_component(
        &self,
        entity_id: &str,
        component_id: &str,
    ) -> Result<WorkbenchComponentResult, WorkbenchFailure> {
        let Some(native_component_id) = self.component_descriptor(entity_id, component_id)? else {
            return Ok(invalid_component_descriptor_result());
        };
        self.component_operation(
            "RST_WorkbenchInspectComponent",
            json!({"entityId":entity_id,"componentId":native_component_id}),
            Some(component_id),
        )
    }
    pub fn add_component(
        &self,
        entity_id: &str,
        class_name: &str,
    ) -> Result<WorkbenchComponentResult, WorkbenchFailure> {
        self.component_operation(
            "RST_WorkbenchAddComponent",
            json!({"entityId":entity_id,"className":class_name}),
            None,
        )
    }
    pub fn remove_component(
        &self,
        entity_id: &str,
        component_id: &str,
        confirmation_token: Option<&str>,
    ) -> Result<WorkbenchComponentResult, WorkbenchFailure> {
        let Some(native_component_id) = self.component_descriptor(entity_id, component_id)? else {
            return Ok(invalid_component_descriptor_result());
        };
        let bound = format!("{entity_id}|{component_id}|{native_component_id}");
        if let Some(token) = confirmation_token {
            let valid = self
                .delete_confirmations
                .lock()
                .unwrap()
                .remove(token)
                .is_some_and(|(value, issued)| {
                    value == bound && issued.elapsed() <= Duration::from_secs(30)
                });
            if !valid {
                return Ok(WorkbenchComponentResult {
                    bridge_version: WORKBENCH_BRIDGE_VERSION.to_string(),
                    protocol_version: WORKBENCH_BRIDGE_PROTOCOL_VERSION,
                    status: "invalid-confirmation".to_string(),
                    entity: None,
                    components: Vec::new(),
                    properties: Vec::new(),
                    confirmation_token: None,
                });
            }
            return self.component_operation(
                "RST_WorkbenchRemoveComponent",
                json!({"entityId":entity_id,"componentId":native_component_id,"confirm":true}),
                None,
            );
        }
        let mut preview = self.component_operation(
            "RST_WorkbenchRemoveComponent",
            json!({"entityId":entity_id,"componentId":native_component_id,"confirm":false}),
            None,
        )?;
        if preview.status == "confirmation-required" {
            let token = format!(
                "cmpdel1:{}",
                sha256(
                    format!(
                        "{bound}:{}",
                        DELETE_CONFIRMATION_SEQUENCE.fetch_add(1, Ordering::Relaxed)
                    )
                    .as_bytes()
                )
            );
            self.delete_confirmations
                .lock()
                .unwrap()
                .insert(token.clone(), (bound, Instant::now()));
            preview.confirmation_token = Some(token);
        }
        Ok(preview)
    }
    pub fn set_component_property(
        &self,
        entity_id: &str,
        component_id: &str,
        write_descriptor: &str,
        value: Value,
    ) -> Result<WorkbenchComponentResult, WorkbenchFailure> {
        let Some(descriptor) =
            self.take_property_descriptor(entity_id, Some(component_id), write_descriptor)
        else {
            return Ok(invalid_component_property_descriptor_result());
        };
        let Some(value) = property_value_wire_format(&descriptor.data_type, &value) else {
            return Ok(invalid_component_property_descriptor_result());
        };
        let Some(native_component_id) = self.component_descriptor(entity_id, component_id)? else {
            return Ok(invalid_component_descriptor_result());
        };
        self.component_operation("RST_WorkbenchSetComponentProperty", json!({"entityId":entity_id,"componentId":native_component_id,"propertyName":descriptor.property_name,"expectedValue":descriptor.observed_value,"value":value}), Some(component_id))
    }
    fn component_operation(
        &self,
        api_func: &str,
        mut request: Value,
        inspected_component_id: Option<&str>,
    ) -> Result<WorkbenchComponentResult, WorkbenchFailure> {
        let started = Instant::now();
        let entity_id = request["entityId"].as_str().unwrap_or_default().to_string();
        request["APIFunc"] = Value::String(api_func.to_string());
        let audit_request = request.clone();
        let raw: RawBridgeComponentResult = serde_json::from_value(
            self.gateway
                .request(request, self.options.gateway.status_deadline)?,
        )
        .map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        if raw.protocol_version != WORKBENCH_BRIDGE_PROTOCOL_VERSION {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        let components = parse_components(&raw.components)
            .map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        let mut properties = parse_properties(&raw.properties)
            .map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        if let Some(component_id) = inspected_component_id {
            self.issue_property_descriptors(
                &entity_id,
                Some(component_id),
                &raw.properties,
                &mut properties,
            );
        }
        let result = WorkbenchComponentResult {
            bridge_version: raw.bridge_version,
            protocol_version: raw.protocol_version,
            status: raw.status,
            entity: parse_optional_world_selection_record(&raw.entity)
                .map_err(|_| failure(WorkbenchFailureCode::Protocol))?,
            components,
            properties,
            confirmation_token: None,
        };
        if let Some(operation) = component_mutation_operation(api_func) {
            self.log_event_timed(
                operation,
                &result.status,
                started,
                component_mutation_audit_details(&audit_request, &result),
            );
        }
        Ok(result)
    }

    fn component_descriptor(
        &self,
        _entity_id: &str,
        descriptor_id: &str,
    ) -> Result<Option<String>, WorkbenchFailure> {
        Ok(is_native_component_descriptor(descriptor_id).then(|| descriptor_id.to_string()))
    }

    pub fn list_entity_properties(
        &self,
        entity_id: &str,
    ) -> Result<WorkbenchPropertyList, WorkbenchFailure> {
        let value = self.gateway.request(
            json!({"APIFunc":"RST_WorkbenchListEntityProperties","entityId":entity_id}),
            self.options.gateway.status_deadline,
        )?;
        let raw: RawBridgePropertyList =
            serde_json::from_value(value).map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        if raw.protocol_version != WORKBENCH_BRIDGE_PROTOCOL_VERSION {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        let mut properties = parse_properties(&raw.properties)
            .map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        self.issue_property_descriptors(entity_id, None, &raw.properties, &mut properties);
        Ok(WorkbenchPropertyList {
            bridge_version: raw.bridge_version,
            protocol_version: raw.protocol_version,
            status: raw.status,
            properties,
        })
    }

    pub fn inspect_prefab_context(
        &self,
        entity_id: Option<&str>,
        resource_name: Option<&str>,
        member_id: Option<&str>,
    ) -> Result<WorkbenchPrefabContext, WorkbenchFailure> {
        if entity_id.is_some() == resource_name.is_some()
            || (member_id.is_some() && resource_name.is_none())
        {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        let value = self.gateway.request(
            json!({"APIFunc":"RST_WorkbenchInspectPrefab","entityId":entity_id.unwrap_or_default(),"resourceName":resource_name.unwrap_or_default(),"memberId":member_id.unwrap_or_default()}),
            self.options.gateway.status_deadline,
        )?;
        let raw: RawBridgePrefabContext =
            serde_json::from_value(value).map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        if raw.bridge_version != WORKBENCH_BRIDGE_VERSION
            || raw.protocol_version != WORKBENCH_BRIDGE_PROTOCOL_VERSION
        {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        let raw_properties = raw.properties.clone();
        Ok(WorkbenchPrefabContext {
            bridge_version: raw.bridge_version,
            protocol_version: raw.protocol_version,
            status: raw.status,
            entity: parse_optional_world_selection_record(&raw.entity)
                .map_err(|_| failure(WorkbenchFailureCode::Protocol))?,
            resource_name: (!raw.resource_name.is_empty()).then_some(raw.resource_name),
            resource_reference_kind: (!raw.resource_reference_kind.is_empty())
                .then_some(raw.resource_reference_kind),
            contributor_addons: split_bounded_records(&raw.contributor_addons),
            ancestor_resources: split_bounded_records(&raw.ancestor_resources),
            ancestor_resources_truncated: workbench_bool(&raw.ancestor_resources_truncated),
            prefab_edit_mode: workbench_bool(&raw.prefab_edit_mode),
            member_id: (!raw.member_id.is_empty()).then_some(raw.member_id.clone()),
            components: parse_component_summaries(&raw.components, &raw.component_properties)
                .map_err(|_| failure(WorkbenchFailureCode::Protocol))?,
            children: parse_prefab_members(&raw.children, &raw.member_id)
                .map_err(|_| failure(WorkbenchFailureCode::Protocol))?,
            children_truncated: workbench_bool(&raw.children_truncated),
            properties: parse_prefab_properties(&raw.properties)
                .map_err(|_| failure(WorkbenchFailureCode::Protocol))?,
            properties_truncated: workbench_bool(&raw.properties_truncated),
            child_count: raw.child_count,
        })
        .map(|mut context| {
            if let Some(target_id) = context
                .entity
                .as_ref()
                .map(|entity| entity.entity_id.as_str())
                .or(context.resource_name.as_deref())
            {
                self.issue_prefab_property_descriptors(
                    target_id,
                    &raw_properties,
                    &mut context.properties,
                );
            }
            context
        })
    }

    pub fn inspect_prefab_component(
        &self,
        entity_id: Option<&str>,
        resource_name: Option<&str>,
        component_id: &str,
        member_id: Option<&str>,
    ) -> Result<WorkbenchPrefabComponentInspection, WorkbenchFailure> {
        if entity_id.is_some() == resource_name.is_some()
            || (member_id.is_some() && resource_name.is_none())
        {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        let value = self.gateway.request(
            json!({"APIFunc":"RST_WorkbenchInspectPrefab","entityId":entity_id.unwrap_or_default(),"resourceName":resource_name.unwrap_or_default(),"memberId":member_id.unwrap_or_default()}),
            self.options.gateway.status_deadline,
        )?;
        let raw: RawBridgePrefabContext =
            serde_json::from_value(value).map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        if raw.bridge_version != WORKBENCH_BRIDGE_VERSION
            || raw.protocol_version != WORKBENCH_BRIDGE_PROTOCOL_VERSION
        {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        let mut component = parse_prefab_components(&raw.components, &raw.component_properties)
            .map_err(|_| failure(WorkbenchFailureCode::Protocol))?
            .into_iter()
            .find(|component| component.component_id == component_id);
        if let (Some(target_id), Some(component)) =
            (entity_id.or(resource_name), component.as_mut())
        {
            self.issue_prefab_component_property_descriptors(
                target_id,
                component_id,
                &raw.component_properties,
                &mut component.properties,
            );
        }
        Ok(WorkbenchPrefabComponentInspection {
            bridge_version: raw.bridge_version,
            protocol_version: raw.protocol_version,
            status: if component.is_some() {
                raw.status
            } else if raw.status == "available" {
                "component-not-found".to_string()
            } else {
                raw.status
            },
            resource_name: raw.resource_name,
            member_id: (!raw.member_id.is_empty()).then_some(raw.member_id),
            component,
        })
    }

    fn issue_prefab_property_descriptors(
        &self,
        entity_id: &str,
        raw: &str,
        properties: &mut [WorkbenchPrefabProperty],
    ) {
        let mut direct = parse_properties(raw).unwrap_or_default();
        self.issue_property_descriptors(entity_id, None, raw, &mut direct);
        for property in properties {
            property.write_descriptor = direct
                .iter()
                .find(|value| value.name == property.path)
                .and_then(|value| value.write_descriptor.clone());
        }
    }

    fn issue_prefab_component_property_descriptors(
        &self,
        entity_id: &str,
        component_id: &str,
        raw_component_properties: &str,
        properties: &mut [WorkbenchPrefabProperty],
    ) {
        let Some(component_index) = component_id
            .splitn(3, ':')
            .nth(1)
            .and_then(|index| index.parse::<u32>().ok())
        else {
            return;
        };
        let raw_properties = raw_component_properties
            .split(';')
            .filter_map(|record| {
                let (index, property) = record.split_once('|')?;
                (index.parse::<u32>().ok() == Some(component_index)).then_some(property)
            })
            .collect::<Vec<_>>()
            .join(";");
        let mut direct = parse_properties(&raw_properties).unwrap_or_default();
        self.issue_property_descriptors(
            entity_id,
            Some(component_id),
            &raw_properties,
            &mut direct,
        );
        for property in properties {
            property.write_descriptor = direct
                .iter()
                .find(|value| value.name == property.path)
                .and_then(|value| value.write_descriptor.clone());
        }
    }

    pub fn create_prefab(
        &self,
        entity_id: &str,
        destination: &str,
        confirmation_token: Option<&str>,
    ) -> Result<WorkbenchEntityMutationResult, WorkbenchFailure> {
        let bound = format!("prefab-create|{entity_id}|{destination}");
        if let Some(token) = confirmation_token {
            let valid = self
                .delete_confirmations
                .lock()
                .unwrap()
                .remove(token)
                .is_some_and(|(value, issued)| {
                    value == bound && issued.elapsed() <= Duration::from_secs(30)
                });
            if !valid {
                return Ok(invalid_confirmation_result());
            }
            return self.entity_mutation(
                "RST_WorkbenchCreatePrefab",
                json!({"entityId":entity_id,"name":destination,"confirm":true}),
            );
        }
        let mut preview = self.entity_mutation(
            "RST_WorkbenchCreatePrefab",
            json!({"entityId":entity_id,"name":destination,"confirm":false}),
        )?;
        if preview.status == "confirmation-required" {
            let token = format!(
                "prefabcreate1:{}",
                sha256(
                    format!(
                        "{bound}:{}",
                        DELETE_CONFIRMATION_SEQUENCE.fetch_add(1, Ordering::Relaxed)
                    )
                    .as_bytes()
                )
            );
            self.delete_confirmations
                .lock()
                .unwrap()
                .insert(token.clone(), (bound, Instant::now()));
            preview.confirmation_token = Some(token);
        }
        Ok(preview)
    }

    pub fn create_generic_prefab(
        &self,
        destination: &str,
        confirmation_token: Option<&str>,
    ) -> Result<WorkbenchEntityMutationResult, WorkbenchFailure> {
        let bound = format!("generic-prefab-create|{destination}");
        if let Some(token) = confirmation_token {
            let valid = self
                .delete_confirmations
                .lock()
                .unwrap()
                .remove(token)
                .is_some_and(|(value, issued)| {
                    value == bound && issued.elapsed() <= Duration::from_secs(30)
                });
            if !valid {
                return Ok(invalid_confirmation_result());
            }
            return self.entity_mutation(
                "RST_WorkbenchCreateGenericPrefab",
                json!({"name":destination,"confirm":true}),
            );
        }
        let mut preview = self.entity_mutation(
            "RST_WorkbenchCreateGenericPrefab",
            json!({"name":destination,"confirm":false}),
        )?;
        if preview.status == "confirmation-required" {
            let token = format!(
                "genericprefabcreate1:{}",
                sha256(
                    format!(
                        "{bound}:{}",
                        DELETE_CONFIRMATION_SEQUENCE.fetch_add(1, Ordering::Relaxed)
                    )
                    .as_bytes()
                )
            );
            self.delete_confirmations
                .lock()
                .unwrap()
                .insert(token.clone(), (bound, Instant::now()));
            preview.confirmation_token = Some(token);
        }
        Ok(preview)
    }

    pub fn save_prefab(
        &self,
        entity_id: Option<&str>,
        resource_name: Option<&str>,
        confirmation_token: Option<&str>,
    ) -> Result<WorkbenchEntityMutationResult, WorkbenchFailure> {
        if entity_id.is_some() == resource_name.is_some()
            || resource_name.is_some_and(|name| !canonical_resource_name(name))
        {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        let target = entity_id
            .or(resource_name)
            .expect("one prefab target is required");
        let bound = format!("prefab-save|{target}");
        if let Some(token) = confirmation_token {
            let valid = self
                .delete_confirmations
                .lock()
                .unwrap()
                .remove(token)
                .is_some_and(|(value, issued)| {
                    value == bound && issued.elapsed() <= Duration::from_secs(30)
                });
            if !valid {
                return Ok(invalid_confirmation_result());
            }
            let mut result = self.entity_mutation(
                "RST_WorkbenchSavePrefab",
                json!({"entityId":entity_id.unwrap_or_default(),"resourceName":resource_name.unwrap_or_default(),"confirm":true}),
            )?;
            if let Some(resource_name) = resource_name.filter(|_| result.status == "prefab-saved") {
                result.resource_name = Some(resource_name.to_string());
                result.persistence_path = Some("workbench-resource".to_string());
                result.template_saved = Some(true);
                result.inspection =
                    Some(self.inspect_prefab_context(None, Some(resource_name), None)?);
            }
            return Ok(result);
        }
        let mut preview = self.entity_mutation(
            "RST_WorkbenchSavePrefab",
            json!({"entityId":entity_id.unwrap_or_default(),"resourceName":resource_name.unwrap_or_default(),"confirm":false}),
        )?;
        if preview.status == "confirmation-required" {
            let token = format!(
                "prefabsave1:{}",
                sha256(
                    format!(
                        "{bound}:{}",
                        DELETE_CONFIRMATION_SEQUENCE.fetch_add(1, Ordering::Relaxed)
                    )
                    .as_bytes()
                )
            );
            self.delete_confirmations
                .lock()
                .unwrap()
                .insert(token.clone(), (bound, Instant::now()));
            preview.confirmation_token = Some(token);
        }
        Ok(preview)
    }

    pub fn add_prefab_resource_component(
        &self,
        resource_name: &str,
        class_name: &str,
        confirmation_token: Option<&str>,
    ) -> Result<WorkbenchPrefabResourceMutationResult, WorkbenchFailure> {
        if !canonical_resource_name(resource_name) || !canonical_component_class_name(class_name) {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }

        let bound = format!("prefab-resource-component-add|{resource_name}|{class_name}");
        let confirm = if let Some(token) = confirmation_token {
            let valid = self
                .delete_confirmations
                .lock()
                .unwrap()
                .remove(token)
                .is_some_and(|(value, issued)| {
                    value == bound && issued.elapsed() <= Duration::from_secs(30)
                });
            if !valid {
                return Ok(invalid_prefab_resource_mutation_result(resource_name));
            }
            true
        } else {
            false
        };

        let raw: RawBridgePrefabResourceComponentMutation =
            serde_json::from_value(self.gateway.request(
                json!({
                    "APIFunc": "RST_WorkbenchAddPrefabResourceComponent",
                    "resourceName": resource_name,
                    "className": class_name,
                    "confirm": confirm,
                }),
                self.options.gateway.status_deadline,
            )?)
            .map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        if raw.bridge_version != WORKBENCH_BRIDGE_VERSION
            || raw.protocol_version != WORKBENCH_BRIDGE_PROTOCOL_VERSION
        {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }

        let mut result = WorkbenchPrefabResourceMutationResult {
            bridge_version: raw.bridge_version,
            protocol_version: raw.protocol_version,
            status: raw.status,
            resource_name: raw.resource_name,
            persistence_path: "workbench-resource".to_string(),
            component_id: (raw.component_index >= 0 && !raw.component_class.is_empty())
                .then(|| format!("cmp1:{}:{}", raw.component_index, raw.component_class)),
            component_class: (!raw.component_class.is_empty()).then_some(raw.component_class),
            template_saved: raw.template_saved,
            inspection: None,
            component_inspection: None,
            confirmation_token: None,
        };
        if result.status == "confirmation-required" {
            let token = format!(
                "prefabresourceadd1:{}",
                sha256(
                    format!(
                        "{bound}:{}",
                        DELETE_CONFIRMATION_SEQUENCE.fetch_add(1, Ordering::Relaxed)
                    )
                    .as_bytes()
                )
            );
            self.delete_confirmations
                .lock()
                .unwrap()
                .insert(token.clone(), (bound, Instant::now()));
            result.confirmation_token = Some(token);
        }
        if result.status == "prefab-component-added" && result.template_saved {
            result.inspection =
                Some(self.inspect_prefab_context(None, Some(resource_name), None)?);
        }
        Ok(result)
    }

    pub fn remove_prefab_resource_component(
        &self,
        resource_name: &str,
        component_id: &str,
        confirmation_token: Option<&str>,
    ) -> Result<WorkbenchPrefabResourceMutationResult, WorkbenchFailure> {
        if !canonical_resource_name(resource_name) {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        let Some((component_index, class_name)) = parse_prefab_component_id(component_id) else {
            return Err(failure(WorkbenchFailureCode::Protocol));
        };

        let bound = format!("prefab-resource-component-remove|{resource_name}|{component_id}");
        let confirm = if let Some(token) = confirmation_token {
            let valid = self
                .delete_confirmations
                .lock()
                .unwrap()
                .remove(token)
                .is_some_and(|(value, issued)| {
                    value == bound && issued.elapsed() <= Duration::from_secs(30)
                });
            if !valid {
                return Ok(invalid_prefab_resource_mutation_result(resource_name));
            }
            true
        } else {
            false
        };

        let raw: RawBridgePrefabResourceComponentMutation =
            serde_json::from_value(self.gateway.request(
                json!({
                    "APIFunc": "RST_WorkbenchRemovePrefabResourceComponent",
                    "resourceName": resource_name,
                    "className": class_name,
                    "componentIndex": component_index,
                    "confirm": confirm,
                }),
                self.options.gateway.status_deadline,
            )?)
            .map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        if raw.bridge_version != WORKBENCH_BRIDGE_VERSION
            || raw.protocol_version != WORKBENCH_BRIDGE_PROTOCOL_VERSION
        {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }

        let mut result = WorkbenchPrefabResourceMutationResult {
            bridge_version: raw.bridge_version,
            protocol_version: raw.protocol_version,
            status: raw.status,
            resource_name: raw.resource_name,
            persistence_path: "workbench-resource".to_string(),
            component_id: Some(component_id.to_string()),
            component_class: (!raw.component_class.is_empty()).then_some(raw.component_class),
            template_saved: raw.template_saved,
            inspection: None,
            component_inspection: None,
            confirmation_token: None,
        };
        if result.status == "confirmation-required" {
            let token = format!(
                "prefabresourceremove1:{}",
                sha256(
                    format!(
                        "{bound}:{}",
                        DELETE_CONFIRMATION_SEQUENCE.fetch_add(1, Ordering::Relaxed)
                    )
                    .as_bytes()
                )
            );
            self.delete_confirmations
                .lock()
                .unwrap()
                .insert(token.clone(), (bound, Instant::now()));
            result.confirmation_token = Some(token);
        }
        if result.status == "prefab-component-removed" && result.template_saved {
            result.inspection =
                Some(self.inspect_prefab_context(None, Some(resource_name), None)?);
        }
        Ok(result)
    }

    pub fn set_prefab_resource_property(
        &self,
        resource_name: &str,
        component_id: Option<&str>,
        write_descriptor: &str,
        value: Value,
        confirmation_token: Option<&str>,
    ) -> Result<WorkbenchPrefabResourceMutationResult, WorkbenchFailure> {
        if !canonical_resource_name(resource_name) {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        let component = match component_id {
            Some(component_id) => Some(
                parse_prefab_component_id(component_id)
                    .ok_or_else(|| failure(WorkbenchFailureCode::Protocol))?,
            ),
            None => None,
        };
        let component_class = component
            .as_ref()
            .map(|(_, class_name)| class_name.as_str());
        let component_index = component.as_ref().map_or(-1, |(index, _)| *index);
        let component_id = component_id.unwrap_or_default();
        let bound_target = format!("{resource_name}|{component_id}|{write_descriptor}");

        let confirm = confirmation_token.is_some();
        let descriptor = self.peek_property_descriptor(
            resource_name,
            (!component_id.is_empty()).then_some(component_id),
            write_descriptor,
        );
        let Some(mut descriptor) = descriptor else {
            return Ok(invalid_prefab_resource_mutation_result(resource_name));
        };
        let Some(value) = property_value_wire_format(&descriptor.data_type, &value) else {
            return Ok(invalid_prefab_resource_mutation_result(resource_name));
        };
        let bound = format!(
            "prefab-resource-property|{bound_target}|{}",
            sha256(value.as_bytes())
        );

        if let Some(token) = confirmation_token {
            let valid = self
                .delete_confirmations
                .lock()
                .unwrap()
                .remove(token)
                .is_some_and(|(stored, issued)| {
                    stored == bound && issued.elapsed() <= Duration::from_secs(30)
                });
            if !valid {
                return Ok(invalid_prefab_resource_mutation_result(resource_name));
            }
            let Some(taken_descriptor) = self.take_property_descriptor(
                resource_name,
                (!component_id.is_empty()).then_some(component_id),
                write_descriptor,
            ) else {
                return Ok(invalid_prefab_resource_mutation_result(resource_name));
            };
            descriptor = taken_descriptor;
        }

        let raw: RawBridgePrefabResourcePropertyMutation =
            serde_json::from_value(self.gateway.request(
                json!({
                    "APIFunc": "RST_WorkbenchSetPrefabResourceProperty",
                    "resourceName": resource_name,
                    "componentIndex": component_index,
                    "componentClass": component_class.unwrap_or_default(),
                    "propertyName": descriptor.property_name,
                    "expectedValue": descriptor.observed_value,
                    "value": value,
                    "confirm": confirm,
                }),
                self.options.gateway.status_deadline,
            )?)
            .map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        if raw.bridge_version != WORKBENCH_BRIDGE_VERSION
            || raw.protocol_version != WORKBENCH_BRIDGE_PROTOCOL_VERSION
        {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }

        let mut result = WorkbenchPrefabResourceMutationResult {
            bridge_version: raw.bridge_version,
            protocol_version: raw.protocol_version,
            status: raw.status,
            resource_name: raw.resource_name,
            persistence_path: "workbench-resource".to_string(),
            component_id: (!component_id.is_empty()).then_some(component_id.to_string()),
            component_class: component_class.map(str::to_string),
            template_saved: raw.template_saved,
            inspection: None,
            component_inspection: None,
            confirmation_token: None,
        };
        if result.status == "confirmation-required" {
            let token = format!(
                "prefabresourceproperty1:{}",
                sha256(
                    format!(
                        "{bound}:{}",
                        DELETE_CONFIRMATION_SEQUENCE.fetch_add(1, Ordering::Relaxed)
                    )
                    .as_bytes()
                )
            );
            self.delete_confirmations
                .lock()
                .unwrap()
                .insert(token.clone(), (bound, Instant::now()));
            result.confirmation_token = Some(token);
        }
        if result.status == "prefab-property-set" && result.template_saved {
            if component_id.is_empty() {
                result.inspection =
                    Some(self.inspect_prefab_context(None, Some(resource_name), None)?);
            } else {
                result.component_inspection = Some(self.inspect_prefab_component(
                    None,
                    Some(resource_name),
                    component_id,
                    None,
                )?);
            }
        }
        Ok(result)
    }

    pub fn set_prefab_property(
        &self,
        entity_id: &str,
        write_descriptor: &str,
        value: Value,
    ) -> Result<WorkbenchEntityMutationResult, WorkbenchFailure> {
        let Some(descriptor) = self.take_property_descriptor(entity_id, None, write_descriptor)
        else {
            return Ok(invalid_property_descriptor_result());
        };
        let Some(value) = property_value_wire_format(&descriptor.data_type, &value) else {
            return Ok(invalid_property_descriptor_result());
        };
        self.entity_mutation("RST_WorkbenchSetPrefabProperty", json!({"entityId":entity_id,"propertyName":descriptor.property_name,"expectedValue":descriptor.observed_value,"value":value}))
    }
    pub fn set_prefab_component_property(
        &self,
        entity_id: &str,
        component_id: &str,
        write_descriptor: &str,
        value: Value,
    ) -> Result<WorkbenchComponentResult, WorkbenchFailure> {
        let Some(descriptor) =
            self.take_property_descriptor(entity_id, Some(component_id), write_descriptor)
        else {
            return Ok(invalid_component_property_descriptor_result());
        };
        let Some(value) = property_value_wire_format(&descriptor.data_type, &value) else {
            return Ok(invalid_component_property_descriptor_result());
        };
        let Some(native_component_id) = self.component_descriptor(entity_id, component_id)? else {
            return Ok(invalid_component_descriptor_result());
        };
        self.component_operation("RST_WorkbenchSetPrefabComponentProperty", json!({"entityId":entity_id,"componentId":native_component_id,"propertyName":descriptor.property_name,"expectedValue":descriptor.observed_value,"value":value}), Some(component_id))
    }
    pub fn set_entity_property(
        &self,
        entity_id: &str,
        write_descriptor: &str,
        value: Value,
    ) -> Result<WorkbenchEntityMutationResult, WorkbenchFailure> {
        let Some(descriptor) = self.take_property_descriptor(entity_id, None, write_descriptor)
        else {
            return Ok(invalid_property_descriptor_result());
        };
        let Some(value) = property_value_wire_format(&descriptor.data_type, &value) else {
            return Ok(invalid_property_descriptor_result());
        };
        self.entity_mutation("RST_WorkbenchSetEntityProperty", json!({"entityId":entity_id,"propertyName":descriptor.property_name,"expectedValue":descriptor.observed_value,"value":value}))
    }

    fn issue_property_descriptors(
        &self,
        entity_id: &str,
        component_id: Option<&str>,
        raw_properties: &str,
        properties: &mut [WorkbenchDirectProperty],
    ) {
        let raw_values = raw_properties
            .split(';')
            .filter_map(|record| {
                let mut fields = record.split('|');
                let name = fields.next()?;
                let data_type = fields.next()?;
                let value = fields.next()?;
                fields.next();
                Some((name, (data_type, value)))
            })
            .collect::<HashMap<_, _>>();
        let Ok(mut descriptors) = self.property_write_descriptors.lock() else {
            return;
        };
        descriptors.retain(|_, descriptor| descriptor.issued.elapsed() <= Duration::from_secs(30));
        for property in properties {
            let Some((data_type, observed_value)) = raw_values.get(property.name.as_str()).copied()
            else {
                continue;
            };
            if !supported_property_type(data_type) {
                continue;
            }
            let sequence = PROPERTY_DESCRIPTOR_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let descriptor_id = format!(
                "prop2:{}",
                sha256(
                    format!(
                        "{entity_id}|{}|{}|{data_type}|{observed_value}|{sequence}",
                        component_id.unwrap_or_default(),
                        property.name
                    )
                    .as_bytes()
                )
            );
            descriptors.insert(
                descriptor_id.clone(),
                PropertyWriteDescriptor {
                    entity_id: entity_id.to_string(),
                    component_id: component_id.map(str::to_string),
                    property_name: property.name.clone(),
                    data_type: data_type.to_string(),
                    observed_value: observed_value.to_string(),
                    issued: Instant::now(),
                },
            );
            property.write_descriptor = Some(descriptor_id);
        }
    }

    fn take_property_descriptor(
        &self,
        entity_id: &str,
        component_id: Option<&str>,
        descriptor_id: &str,
    ) -> Option<PropertyWriteDescriptor> {
        let mut descriptors = self.property_write_descriptors.lock().ok()?;
        let descriptor = descriptors.remove(descriptor_id)?;
        (descriptor.issued.elapsed() <= Duration::from_secs(30)
            && descriptor.entity_id == entity_id
            && descriptor.component_id.as_deref() == component_id)
            .then_some(descriptor)
    }

    fn peek_property_descriptor(
        &self,
        entity_id: &str,
        component_id: Option<&str>,
        descriptor_id: &str,
    ) -> Option<PropertyWriteDescriptor> {
        let mut descriptors = self.property_write_descriptors.lock().ok()?;
        descriptors.retain(|_, descriptor| descriptor.issued.elapsed() <= Duration::from_secs(30));
        descriptors
            .get(descriptor_id)
            .cloned()
            .filter(|descriptor| {
                descriptor.entity_id == entity_id
                    && descriptor.component_id.as_deref() == component_id
            })
    }

    fn selection_mutation(
        &self,
        api_func: &str,
        mut request: Value,
    ) -> Result<WorkbenchWorldSelectionSummary, WorkbenchFailure> {
        request["APIFunc"] = Value::String(api_func.to_string());
        let value = self
            .gateway
            .request(request, self.options.gateway.status_deadline)?;
        let raw: RawBridgeWorldSelection =
            serde_json::from_value(value).map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        if raw.protocol_version != WORKBENCH_BRIDGE_PROTOCOL_VERSION {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        let selected_entities = parse_world_selection_records(&raw.selected_entities)
            .map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        if selected_entities.len() > 32 || selected_entities.len() > raw.selected_count as usize {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        Ok(WorkbenchWorldSelectionSummary {
            bridge_version: raw.bridge_version,
            protocol_version: raw.protocol_version,
            editor_available: workbench_bool(&raw.editor_available),
            status: raw.status,
            selected_count: raw.selected_count,
            selected_entities_truncated: workbench_bool(&raw.selected_entities_truncated)
                || selected_entities.len() < raw.selected_count as usize,
            selected_entities,
        })
    }

    fn dispatch_background_reload_action(&self) -> Result<RawBridgeReloadAction, WorkbenchFailure> {
        let value = self.gateway.request(
            json!({"APIFunc": "RST_WorkbenchState", "executeReloadAction": true}),
            self.options.gateway.status_deadline,
        )?;
        let raw: RawBridgeReloadAction =
            serde_json::from_value(value).map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        // Reload is the one recovery action that must remain callable from the
        // immediately previous compatible handler after its managed package is upgraded.
        if raw.protocol_version != WORKBENCH_BRIDGE_PROTOCOL_VERSION
            || raw.action_path != "Plugins/Settings/Reload WB Scripts"
        {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        Ok(raw)
    }

    fn dispatch_background_save_all_action(
        &self,
    ) -> Result<RawBridgeSaveAllAction, WorkbenchFailure> {
        let value = self.gateway.request(
            json!({"APIFunc": "RST_WorkbenchState", "executeSaveAllAction": true}),
            self.options.gateway.status_deadline,
        )?;
        let raw: RawBridgeSaveAllAction =
            serde_json::from_value(value).map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        if raw.protocol_version != WORKBENCH_BRIDGE_PROTOCOL_VERSION
            || raw.action_path != "File/Save All"
        {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        Ok(raw)
    }

    /// Dispatch the fixed Workbench Save All action in-process and wait briefly for it to settle.
    pub fn save(&self) -> Result<WorkbenchSaveResult, WorkbenchFailure> {
        const POST_SAVE_ACTION_DELAY: Duration = Duration::from_millis(750);

        let started = Instant::now();
        let action = self
            .dispatch_background_save_all_action()
            .map_err(|dispatch_failure| {
                self.correlate_failure_details(
                    "save-all",
                    "workbench-save-all-action-unavailable",
                    dispatch_failure,
                    json!({"handler": "RST_WorkbenchState"}),
                )
            })?;
        if !workbench_bool(&action.accepted)
            || (action.world_save_status == "saved" && !workbench_bool(&action.world_save_accepted))
            || !matches!(
                action.world_save_status.as_str(),
                "saved" | "skipped-no-open-world"
            )
        {
            return Err(self.correlate_failure_details(
                "save-all",
                "workbench-save-all-or-world-save-rejected",
                failure(WorkbenchFailureCode::Unavailable),
                json!({
                    "actionPath": action.action_path,
                    "saveAllAccepted": workbench_bool(&action.accepted),
                    "worldSaveAccepted": workbench_bool(&action.world_save_accepted),
                    "worldSaveStatus": action.world_save_status,
                }),
            ));
        }
        std::thread::sleep(POST_SAVE_ACTION_DELAY);
        let result = WorkbenchSaveResult {
            save_all_accepted: true,
            world_save_accepted: workbench_bool(&action.world_save_accepted),
            world_save_status: action.world_save_status,
            action_path: action.action_path,
        };
        self.log_event_timed(
            "save-all",
            "accepted",
            started,
            json!({
                "actionPath": result.action_path,
                "worldSaveAccepted": result.world_save_accepted,
                "worldSaveStatus": result.world_save_status,
            }),
        );
        Ok(result)
    }

    /// Dispatch the fixed Workbench reload action in-process after a confirmed Save All action.
    ///
    /// Reload destroys this handler before it can report completion. The versioned capability
    /// handshake supplies the typed runtime generation used to prove the replacement loaded.
    pub fn activate_scripts(&self) -> Result<WorkbenchScriptActivationResult, WorkbenchFailure> {
        const RELOAD_VERIFICATION_DEADLINE: Duration = Duration::from_secs(60);
        const RELOAD_VERIFICATION_POLL: Duration = Duration::from_millis(500);

        let started = Instant::now();
        let before = self.capabilities_handshake()?;
        if before.protocol_version != WORKBENCH_BRIDGE_PROTOCOL_VERSION
            || before.runtime_generation == 0
        {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        let save_result = self.save().map_err(|save_failure| {
            self.correlate_failure_details(
                "activate-scripts",
                "workbench-save-all-before-reload-failed",
                save_failure,
                json!({"handler": "RST_WorkbenchState"}),
            )
        })?;
        let world_saved_before_reload = save_result.world_save_accepted;
        let world_save_status = save_result.world_save_status;
        reload_action_path(self.dispatch_background_reload_action()).map_err(
            |dispatch_failure| {
                self.correlate_failure_details(
                    "activate-scripts",
                    "workbench-reload-action-unavailable",
                    dispatch_failure,
                    json!({"handler": "RST_WorkbenchState"}),
                )
            },
        )?;
        while started.elapsed() < RELOAD_VERIFICATION_DEADLINE {
            if let Ok(after) = self.capabilities_handshake() {
                if after.protocol_version == WORKBENCH_BRIDGE_PROTOCOL_VERSION
                    && after.bridge_version == WORKBENCH_BRIDGE_VERSION
                    && after.runtime_generation != 0
                    && after.runtime_generation != before.runtime_generation
                {
                    let result = WorkbenchScriptActivationResult {
                        world_saved_before_reload,
                        world_save_status,
                        reload_dispatched: true,
                        runtime_generation: after.runtime_generation,
                    };
                    self.log_event_timed(
                        "activate-scripts",
                        "verified",
                        started,
                        json!({
                            "worldSavedBeforeReload": result.world_saved_before_reload,
                            "worldSaveStatus": result.world_save_status,
                            "runtimeGeneration": result.runtime_generation,
                        }),
                    );
                    return Ok(result);
                }
            }
            std::thread::sleep(RELOAD_VERIFICATION_POLL);
        }
        Err(self.correlate_failure_details(
            "activate-scripts",
            "typed-generation-not-observed",
            failure(WorkbenchFailureCode::Timeout),
            json!({"previousRuntimeGeneration": before.runtime_generation}),
        ))
    }

    pub fn read_logs(
        &self,
        source: &str,
        mode: &str,
        line_count: Option<usize>,
    ) -> Result<WorkbenchLogRead, WorkbenchFailure> {
        if !matches!(mode, "latest" | "tail" | "all") {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        let path = match source {
            "integration" => Some(self.integration_log_path()),
            "workbench" => latest_workbench_log(&self.paths().workbench_root),
            _ => return Err(failure(WorkbenchFailureCode::Protocol)),
        };
        let Some(path) = path else {
            return Ok(WorkbenchLogRead {
                source: source.to_string(),
                path: None,
                lines: Vec::new(),
                markers: Vec::new(),
                truncated: false,
            });
        };
        if !path.is_file() {
            return Ok(WorkbenchLogRead {
                source: source.to_string(),
                path: Some(path),
                lines: Vec::new(),
                markers: Vec::new(),
                truncated: false,
            });
        }
        let (lines, truncated) = match (source, mode) {
            ("workbench", "latest") => bounded_log_since_reload_start(&path, line_count),
            (_, "tail") => bounded_log_tail(&path, line_count.unwrap_or(200).clamp(1, 500)),
            (_, "all") => read_all_log_lines(&path, line_count.map(|count| count.clamp(1, 500))),
            ("integration", "latest") => {
                bounded_log_tail(&path, line_count.unwrap_or(200).clamp(1, 500))
            }
            _ => unreachable!("validated Workbench log mode"),
        }
        .map_err(|error| {
            self.correlate_failure_details(
                "read_logs",
                "read-failed",
                failure(WorkbenchFailureCode::Unavailable),
                json!({
                    "source": source,
                    "mode": mode,
                    "errorKind": format!("{:?}", error.kind()),
                }),
            )
        })?;
        let result = WorkbenchLogRead {
            source: source.to_string(),
            path: Some(path),
            markers: workbench_log_markers(source, &lines),
            lines,
            truncated,
        };
        self.log_event_timed(
            "read-logs",
            "success",
            Instant::now(),
            json!({
                "source": result.source.clone(),
                "mode": mode,
                "lineCount": result.lines.len(),
                "markerCount": result.markers.len(),
                "truncated": result.truncated,
            }),
        );
        Ok(result)
    }

    pub fn list_windows(&self, process_id: u32) -> Result<WorkbenchWindowList, WorkbenchFailure> {
        let process = self.capture_process(process_id, "list_windows")?;
        let windows = workbench_capture::list_windows(process.id).map_err(|error| {
            self.capture_failure(
                "list_windows",
                process,
                error,
                WorkbenchFailureCode::CaptureUnavailable,
            )
        })?;
        Ok(WorkbenchWindowList {
            process_id: process.id,
            windows,
        })
    }

    pub fn capture_window(
        &self,
        process_id: u32,
        window_id: Option<&str>,
        max_dimension: Option<u32>,
        region: Option<CaptureRegion>,
    ) -> Result<CapturedWindow, WorkbenchFailure> {
        let process = self.capture_process(process_id, "capture_window")?;
        let max_dimension = max_dimension.unwrap_or(DEFAULT_MAX_DIMENSION);
        if !(MIN_MAX_DIMENSION..=MAX_MAX_DIMENSION).contains(&max_dimension) {
            return Err(self.correlate_failure_details(
                "capture_window",
                "invalid-max-dimension",
                failure(WorkbenchFailureCode::CaptureUnavailable),
                json!({
                    "processId": process.id,
                    "maxDimension": max_dimension,
                    "minimum": MIN_MAX_DIMENSION,
                    "maximum": MAX_MAX_DIMENSION,
                }),
            ));
        }
        let _capture_guard = self.capture_lock.lock().map_err(|_| {
            self.correlate_failure_details(
                "capture_window",
                "capture-admission-unavailable",
                failure(WorkbenchFailureCode::CaptureUnavailable),
                json!({"processId": process.id}),
            )
        })?;
        workbench_capture::capture_window(process.id, window_id, max_dimension, region).map_err(
            |error| {
                let code = match error {
                    CaptureError::InvalidRegion => WorkbenchFailureCode::CaptureInvalidRegion,
                    CaptureError::TooLarge => WorkbenchFailureCode::CaptureTooLarge,
                    CaptureError::Unsupported
                    | CaptureError::NoWindow
                    | CaptureError::InvalidWindowId
                    | CaptureError::Minimized
                    | CaptureError::NativeCapture
                    | CaptureError::Encoding => WorkbenchFailureCode::CaptureUnavailable,
                };
                self.capture_failure("capture_window", process, error, code)
            },
        )
    }

    fn capture_process(
        &self,
        process_id: u32,
        operation: &str,
    ) -> Result<ProcessIdentity, WorkbenchFailure> {
        let observed = self.observed_processes.lock().ok().and_then(|processes| {
            processes
                .iter()
                .find(|process| process.id == process_id)
                .copied()
        });
        let current = process::workbench_processes();
        if observed.is_none() || !current.iter().any(|process| Some(*process) == observed) {
            return Err(self.correlate_failure_details(
                operation,
                "stale-or-unobserved-process",
                failure(WorkbenchFailureCode::CaptureUnavailable),
                json!({
                    "processId": process_id,
                    "observedBySession": observed.is_some(),
                    "currentIdentityMatched": false,
                }),
            ));
        }
        Ok(observed.expect("capture process identity was checked"))
    }

    fn capture_failure(
        &self,
        operation: &str,
        process: ProcessIdentity,
        error: CaptureError,
        code: WorkbenchFailureCode,
    ) -> WorkbenchFailure {
        self.correlate_failure_details(
            operation,
            "window-capture-failed",
            failure(code),
            json!({
                "processId": process.id,
                "captureError": error.to_string(),
            }),
        )
    }

    pub fn launch(
        &self,
        project: &std::path::Path,
    ) -> Result<WorkbenchProcessResult, WorkbenchFailure> {
        self.launch_project(Some(project))
    }

    fn launch_project(
        &self,
        project: Option<&std::path::Path>,
    ) -> Result<WorkbenchProcessResult, WorkbenchFailure> {
        let started = Instant::now();
        if let Some(project) = project {
            if !project.is_file()
                || !project
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("gproj"))
            {
                return Err(self.correlate_failure_details(
                    "launch",
                    "project-unavailable",
                    failure(WorkbenchFailureCode::Unavailable),
                    json!({"project": project}),
                ));
            }
        }
        let existing = process::workbench_processes();
        self.observe_processes(&existing);
        if let Some(process) = existing.first() {
            if let Some(requested_project) = project {
                let observed_project = workbench_project_gproj(&self.host, *process);
                if observed_project
                    .as_deref()
                    .is_none_or(|observed| !paths_equal(observed, requested_project))
                {
                    return Err(self.correlate_failure_details(
                        "launch",
                        "existing-process-project-mismatch",
                        failure(WorkbenchFailureCode::Unavailable),
                        json!({
                            "processId": process.id,
                            "requestedProject": requested_project,
                            "observedProject": observed_project,
                        }),
                    ));
                }
            }
            let net_api_connected =
                self.native_status().is_ok() || self.wait_for_net_api(NET_API_READY_DEADLINE);
            if !net_api_connected {
                return Err(self.correlate_failure_details(
                    "launch",
                    "net-api-timeout",
                    failure(WorkbenchFailureCode::Timeout),
                    json!({
                        "processId": process.id,
                        "alreadyRunning": true,
                    }),
                ));
            }
            let result = WorkbenchProcessResult {
                process_id: Some(process.id),
                already_running: true,
                net_api_connected,
                exited: false,
                user_interaction_required: false,
            };
            self.log_event_timed(
                "launch",
                "already-running",
                started,
                json!({"processId": process.id, "netApiConnected": true}),
            );
            return Ok(result);
        }
        let paths = self.paths();
        let executable = paths
            .executable
            .as_ref()
            .filter(|path| is_workbench_executable(path))
            .cloned()
            .ok_or_else(|| {
                self.correlate_failure_details(
                    "launch",
                    "executable-unavailable",
                    failure(WorkbenchFailureCode::Unavailable),
                    json!({"executableValidated": false}),
                )
            })?;
        let profile_root = self
            .options
            .profile_directory
            .as_deref()
            .and_then(std::path::Path::parent);
        let arguments =
            workbench_launch_arguments(&self.host, project, paths.game.as_deref(), profile_root)
                .ok_or_else(|| {
                    self.correlate_failure_details(
                        "launch",
                        "base-game-addon-directory-unavailable",
                        failure(WorkbenchFailureCode::Unavailable),
                        json!({
                            "project": project,
                            "gameDirectoryDiscovered": paths.game.is_some(),
                            "gameDirectorySource": paths.game_source,
                            "workbenchHost": self.host.source(),
                        }),
                    )
                })?;
        let launch = self
            .host
            .workbench_launch(&executable, &arguments)
            .ok_or_else(|| {
                self.correlate_failure_details(
                    "launch",
                    "launch-route-unavailable",
                    failure(WorkbenchFailureCode::Unavailable),
                    json!({"workbenchHost": self.host.source()}),
                )
            })?;
        let launch_source = launch.source;
        let mut command = launch.command;
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        command.spawn().map_err(|error| {
            self.correlate_failure_details(
                "launch",
                "process-start-failed",
                failure(WorkbenchFailureCode::Unavailable),
                json!({
                    "errorKind": format!("{:?}", error.kind()),
                    "launchSource": launch_source,
                }),
            )
        })?;
        // Steam and Wine both hand Workbench off to a process the launcher
        // owns, so the started Workbench is the new process identity rather
        // than the child that was spawned.
        let process = self.wait_for_new_workbench_process(&existing, LAUNCH_PROCESS_DEADLINE);
        if let Some(process) = process {
            self.observe_processes(&[process]);
        }
        let net_api_connected = process.is_some() && self.wait_for_net_api(NET_API_READY_DEADLINE);
        if !net_api_connected {
            return Err(self.correlate_failure_details(
                "launch",
                if process.is_some() {
                    "net-api-timeout"
                } else {
                    "process-not-observed"
                },
                failure(WorkbenchFailureCode::Timeout),
                json!({
                    "processId": process.map(|process| process.id),
                    "alreadyRunning": false,
                    "launchSource": launch_source,
                    "workbenchHost": self.host.source(),
                }),
            ));
        }
        let result = WorkbenchProcessResult {
            process_id: process.map(|process| process.id),
            already_running: false,
            net_api_connected,
            exited: false,
            user_interaction_required: false,
        };
        self.log_event_timed(
            "launch",
            if net_api_connected {
                "connected"
            } else {
                "net-api-timeout"
            },
            started,
            json!({
                "processId": result.process_id,
                "alreadyRunning": result.already_running,
                "netApiConnected": result.net_api_connected,
            }),
        );
        Ok(result)
    }

    fn save_before_process_control(&self, operation: &str, process_id: u32) -> bool {
        const SAVE_CONFIRMATION_DEADLINE: Duration = Duration::from_secs(15);
        const SAVE_CONFIRMATION_POLL: Duration = Duration::from_millis(250);

        let started = Instant::now();
        loop {
            match self.save() {
                Ok(result) => {
                    self.log_event_timed(
                        operation,
                        "save-confirmed",
                        started,
                        json!({
                            "processId": process_id,
                            "saveAllAccepted": result.save_all_accepted,
                            "worldSaveAccepted": result.world_save_accepted,
                            "worldSaveStatus": result.world_save_status,
                        }),
                    );
                    return true;
                }
                Err(_) => {}
            }
            if started.elapsed() >= SAVE_CONFIRMATION_DEADLINE {
                break;
            }
            std::thread::sleep(SAVE_CONFIRMATION_POLL);
        }
        self.log_event_timed(
            operation,
            "save-confirmation-timeout",
            started,
            json!({
                "processId": process_id,
                "deadlineMs": SAVE_CONFIRMATION_DEADLINE.as_millis(),
                "forcedProcessControl": true,
            }),
        );
        false
    }

    pub fn stop(&self, process_id: u32) -> Result<WorkbenchProcessResult, WorkbenchFailure> {
        let started = Instant::now();
        let current = process::workbench_processes();
        let observed = self.observed_processes.lock().ok().and_then(|processes| {
            processes
                .iter()
                .find(|process| process.id == process_id)
                .copied()
        });
        if observed.is_none() || !current.iter().any(|process| Some(*process) == observed) {
            return Err(self.correlate_failure_details(
                "stop",
                "stale-or-unobserved-process",
                failure(WorkbenchFailureCode::Unavailable),
                json!({
                    "processId": process_id,
                    "observedBySession": observed.is_some(),
                    "currentIdentityMatched": false,
                }),
            ));
        }
        let observed = observed.expect("checked observed process identity");
        let save_confirmed = self.save_before_process_control("stop", process_id);
        let close_mode = if save_confirmed { "graceful" } else { "force" };
        process::close(
            observed,
            if save_confirmed {
                CloseMode::Graceful
            } else {
                CloseMode::Force
            },
        )
        .map_err(|error| {
            self.correlate_failure_details(
                "stop",
                if save_confirmed {
                    "graceful-close-failed"
                } else {
                    "force-close-failed"
                },
                failure(WorkbenchFailureCode::Unavailable),
                json!({
                    "processId": process_id,
                    "errorKind": format!("{:?}", error.kind()),
                    "closeMode": close_mode,
                }),
            )
        })?;
        for _ in 0..20 {
            if !process::workbench_process_ids().contains(&process_id) {
                self.log_event_timed(
                    "stop",
                    "exited",
                    started,
                    json!({"processId": process_id, "closeMode": close_mode}),
                );
                return Ok(WorkbenchProcessResult {
                    process_id: Some(process_id),
                    already_running: false,
                    net_api_connected: false,
                    exited: true,
                    user_interaction_required: false,
                });
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        if save_confirmed {
            // A saved Workbench can still keep its main window alive while a modal or
            // editor-owned shutdown path settles. The exact identity was captured above and
            // Save All was confirmed, so finish the public stop operation through the same
            // identity-checked force-close path used when save confirmation is unavailable.
            self.log_event_timed(
                "stop",
                "graceful-close-timeout-falling-back-to-force",
                started,
                json!({
                    "processId": process_id,
                    "closeMode": close_mode,
                }),
            );
            if process::close(observed, CloseMode::Force).is_ok() {
                for _ in 0..20 {
                    if !process::workbench_process_ids().contains(&process_id) {
                        self.log_event_timed(
                            "stop",
                            "exited-after-force-fallback",
                            started,
                            json!({
                                "processId": process_id,
                                "closeMode": "force-after-graceful-timeout",
                            }),
                        );
                        return Ok(WorkbenchProcessResult {
                            process_id: Some(process_id),
                            already_running: false,
                            net_api_connected: false,
                            exited: true,
                            user_interaction_required: false,
                        });
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
        }
        let result = WorkbenchProcessResult {
            process_id: Some(process_id),
            already_running: false,
            net_api_connected: self.native_status().is_ok(),
            exited: false,
            user_interaction_required: true,
        };
        self.log_event_timed(
            "stop",
            "user-interaction-required",
            started,
            json!({
                "processId": process_id,
                "netApiConnected": result.net_api_connected,
                "closeMode": close_mode,
            }),
        );
        Ok(result)
    }

    pub fn restart(&self, process_id: u32) -> Result<WorkbenchProcessResult, WorkbenchFailure> {
        let current = process::workbench_processes();
        let Some(process) = current
            .iter()
            .find(|process| process.id == process_id)
            .copied()
        else {
            return Err(self.correlate_failure_details(
                "restart",
                "workbench-process-not-running",
                failure(WorkbenchFailureCode::Unavailable),
                json!({"processId": process_id}),
            ));
        };
        let paths = self.paths();
        // Workbench itself is the authority on the project it has open, and it
        // answers on every host. The command line and the window title remain
        // for a running Workbench whose bridge cannot answer.
        let project = self
            .loaded_addon_graph()
            .ok()
            .and_then(|graph| graph.current_project_file)
            .filter(|project| project.is_file())
            .or_else(|| workbench_project_gproj(&self.host, process))
            .or_else(|| {
                workbench_project_title(process)
                    .and_then(|title| resolve_project_gproj(&paths.workbench_root, &title))
            })
            .ok_or_else(|| {
                self.correlate_failure_details(
                    "restart",
                    "project-not-resolved",
                    failure(WorkbenchFailureCode::Unavailable),
                    json!({"processId": process_id}),
                )
            })?;
        if base_game_addons_directory(paths.game.as_deref()).is_none() {
            return Err(self.correlate_failure_details(
                "restart",
                "base-game-addon-directory-unavailable",
                failure(WorkbenchFailureCode::Unavailable),
                json!({
                    "processId": process_id,
                    "gameDirectoryDiscovered": paths.game.is_some(),
                }),
            ));
        }
        let save_confirmed = self.save_before_process_control("restart", process_id);
        process::close(process, CloseMode::Force).map_err(|error| {
            self.correlate_failure_details(
                "restart",
                "force-close-failed",
                failure(WorkbenchFailureCode::Unavailable),
                json!({
                    "processId": process_id,
                    "errorKind": format!("{:?}", error.kind()),
                    "saveConfirmed": save_confirmed,
                }),
            )
        })?;
        for _ in 0..20 {
            if !process::workbench_processes().contains(&process) {
                return self.launch_project(Some(&project));
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        Err(self.correlate_failure_details(
            "restart",
            "force-close-not-observed",
            failure(WorkbenchFailureCode::Unavailable),
            json!({
                "processId": process_id,
                "saveConfirmed": save_confirmed,
            }),
        ))
    }

    fn capabilities_handshake(&self) -> Result<RawBridgeCapabilities, WorkbenchFailure> {
        let value = self.gateway.request(
            json!({"APIFunc": "RST_WorkbenchCapabilities"}),
            self.options.gateway.status_deadline,
        )?;
        serde_json::from_value(value).map_err(|_| failure(WorkbenchFailureCode::Protocol))
    }

    fn active_bridge_status(
        &self,
        bridge_directory: &std::path::Path,
        retry_activation: bool,
    ) -> ManagedBridgeStatus {
        let disk = self.bridge_disk_status(bridge_directory);
        let expected_protocol =
            fs::read(bridge_directory.join("reforger-script-tools.manifest.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<BridgeManifest>(&bytes).ok())
                .map(|manifest| manifest.protocol_version);
        let mut raw = self.capabilities_handshake().ok();
        let handshake_matches = |raw: &RawBridgeCapabilities| {
            disk.installed_version
                .as_deref()
                .is_some_and(|version| version == raw.bridge_version)
                && expected_protocol == Some(raw.protocol_version)
        };
        if retry_activation && raw.as_ref().is_none_or(|raw| !handshake_matches(raw)) {
            let _ = self.gateway.validate_scripts();
            raw = self.capabilities_handshake().ok();
        }
        let Some(raw) = raw else {
            return ManagedBridgeStatus {
                activation_required: disk.installed,
                ..disk
            };
        };
        let (capabilities, capabilities_truncated) = split_bounded_list(&raw.capabilities, 32, 64);
        let compatible = raw.protocol_version == WORKBENCH_BRIDGE_PROTOCOL_VERSION;
        let activation_required = disk.installed && !handshake_matches(&raw);
        ManagedBridgeStatus {
            installed: disk.installed,
            installation_available: false,
            maintenance_required: disk.maintenance_required,
            installed_version: disk.installed_version,
            active_version: Some(raw.bridge_version),
            protocol_version: Some(raw.protocol_version),
            compatible,
            activation_required,
            capabilities: if compatible { capabilities } else { Vec::new() },
            capabilities_truncated: compatible && capabilities_truncated,
        }
    }

    fn bridge_disk_status(&self, bridge_directory: &std::path::Path) -> ManagedBridgeStatus {
        let manifest = fs::read(bridge_directory.join("reforger-script-tools.manifest.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<BridgeManifest>(&bytes).ok());
        let installed = manifest.is_some();
        ManagedBridgeStatus {
            installed,
            installation_available: false,
            maintenance_required: self.bridge_needs_maintenance(bridge_directory),
            installed_version: manifest.map(|value| value.bridge_version),
            active_version: None,
            protocol_version: None,
            compatible: false,
            activation_required: installed,
            capabilities: Vec::new(),
            capabilities_truncated: false,
        }
    }

    fn bridge_needs_maintenance(&self, bridge_directory: &std::path::Path) -> bool {
        let Some(manifest) = fs::read(bridge_directory.join("reforger-script-tools.manifest.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<BridgeManifest>(&bytes).ok())
        else {
            return true;
        };
        version_order(&manifest.bridge_version, WORKBENCH_BRIDGE_VERSION).is_lt()
            || (manifest.bridge_version == WORKBENCH_BRIDGE_VERSION
                && (manifest.protocol_version != WORKBENCH_BRIDGE_PROTOCOL_VERSION
                    || !manifest_matches_payload(&manifest)
                    || bridge_payload().iter().any(|(name, content)| {
                        fs::read(bridge_directory.join(name))
                            .ok()
                            .is_none_or(|bytes| sha256(&bytes) != sha256(content.as_bytes()))
                    })))
    }

    #[cfg(test)]
    fn maintain_existing_bridge(&self, bridge_directory: &std::path::Path) -> ManagedBridgeStatus {
        let started = Instant::now();
        let Ok(_maintenance) = self.maintenance_lock.lock() else {
            return self.bridge_disk_status(bridge_directory);
        };
        let repaired = match self.repair_managed_files(bridge_directory) {
            Ok(repaired) => repaired,
            Err(error) => {
                self.log_event_timed(
                    "maintenance",
                    "repair-failed",
                    started,
                    json!({
                        "errorKind": format!("{:?}", error.kind()),
                        "managedFiles": bridge_payload()
                            .iter()
                            .map(|(name, _)| *name)
                            .collect::<Vec<_>>(),
                    }),
                );
                return self.bridge_disk_status(bridge_directory);
            }
        };
        if repaired {
            let _ = self.gateway.validate_scripts();
        }
        // A missing custom NET API function is logged by Workbench as an error.
        // Maintenance and diagnosis must therefore never probe an unregistered
        // handler. Explicit custom operations remain responsible for their own
        // availability result.
        let status = self.bridge_disk_status(bridge_directory);
        self.log_event_timed(
            "maintenance",
            if repaired {
                "updated-reload-required"
            } else {
                "activation-pending"
            },
            started,
            json!({
                "repaired": repaired,
                "installedVersion": status.installed_version.clone(),
                "activeVersion": status.active_version.clone(),
                "protocolVersion": status.protocol_version,
                "managedFileCount": bridge_payload().len(),
                "managedFiles": bridge_payload()
                    .iter()
                    .map(|(name, _)| *name)
                    .collect::<Vec<_>>(),
            }),
        );
        status
    }

    #[cfg(test)]
    fn repair_managed_files(&self, bridge_directory: &std::path::Path) -> std::io::Result<bool> {
        let manifest_path = bridge_directory.join("reforger-script-tools.manifest.json");
        let manifest = fs::read(&manifest_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<BridgeManifest>(&bytes).ok());
        let needs_repair = manifest.as_ref().is_none_or(|manifest| {
            version_order(&manifest.bridge_version, WORKBENCH_BRIDGE_VERSION).is_lt()
                || (manifest.bridge_version == WORKBENCH_BRIDGE_VERSION
                    && (manifest.protocol_version != WORKBENCH_BRIDGE_PROTOCOL_VERSION
                        || !manifest_matches_payload(manifest)
                        || bridge_payload().iter().any(|(name, content)| {
                            fs::read(bridge_directory.join(name))
                                .ok()
                                .is_none_or(|bytes| sha256(&bytes) != sha256(content.as_bytes()))
                        })))
        });
        if needs_repair {
            self.write_managed_files(bridge_directory)?;
        }
        Ok(needs_repair)
    }

    fn migrate_legacy_bridge(
        &self,
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

    fn write_managed_files(&self, bridge_directory: &std::path::Path) -> std::io::Result<()> {
        self.write_managed_payload(bridge_directory, bridge_payload())
    }

    fn write_managed_payload(
        &self,
        bridge_directory: &std::path::Path,
        payload: &[(&str, &str)],
    ) -> std::io::Result<()> {
        if fs::symlink_metadata(bridge_directory)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "managed bridge directory cannot be a symbolic link",
            ));
        }
        fs::create_dir_all(bridge_directory)?;
        let previous = fs::read(bridge_directory.join("reforger-script-tools.manifest.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<BridgeManifest>(&bytes).ok());
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

    fn paths(&self) -> ResolvedWorkbenchPaths {
        let user = self
            .options
            .user_directory
            .clone()
            .or_else(|| self.host.user_directory())
            .unwrap_or_default();
        let profile_source = if self.options.profile_directory.is_some() {
            "explicit"
        } else {
            self.host.source()
        };
        let profile = self
            .options
            .profile_directory
            .clone()
            .unwrap_or_else(|| profile_directory_in(&user));
        let workbench_root = profile
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| profile.clone());
        let scripts_directory = profile.join("scripts");
        let bridge_directory = scripts_directory
            .join("WorkbenchGame")
            .join("reforger-script-tools");
        let legacy_bridge_directory = scripts_directory.join("reforger-script-tools");
        let (game, game_source) = if let Some(game) = self.options.game_directory.clone() {
            (Some(game), "explicit".to_string())
        } else {
            discover_steam_app(REFORGER_GAME_APP_ID).into_path_and_source()
        };
        let (tools, tools_source) = if let Some(tools) = self.options.tools_directory.clone() {
            (Some(tools), "explicit".to_string())
        } else {
            discover_steam_app(REFORGER_TOOLS_APP_ID).into_path_and_source()
        };
        let (executable, executable_source) =
            if let Some(executable) = self.options.executable.clone() {
                (Some(executable), "explicit".to_string())
            } else {
                (
                    tools
                        .as_ref()
                        .map(|tools| tools.join("Workbench").join(WORKBENCH_EXECUTABLE_NAME)),
                    "tools-installation".to_string(),
                )
            };
        ResolvedWorkbenchPaths {
            workbench_root,
            profile,
            profile_source,
            bridge_directory,
            legacy_bridge_directory,
            game,
            game_source,
            tools,
            tools_source,
            executable,
            executable_source,
        }
    }

    /// The support log this integration writes.
    ///
    /// It lives where the host keeps user state. An explicit user directory is
    /// a test and development override and keeps the Windows layout beneath it.
    fn integration_log_path(&self) -> PathBuf {
        self.options
            .user_directory
            .as_ref()
            .map(|user| {
                user.join("AppData")
                    .join("Local")
                    .join("ReforgerScriptTools")
                    .join("logs")
            })
            .or_else(host_platform::support_log_directory)
            .unwrap_or_default()
            .join("workbench.log")
    }

    fn log_event_timed(
        &self,
        operation: &str,
        outcome: &str,
        started: Instant,
        details: Value,
    ) -> String {
        use std::io::Write;
        let reference = next_log_reference();
        let path = self.integration_log_path();
        let Some(parent) = path.parent() else {
            return reference;
        };
        if fs::create_dir_all(parent).is_err() {
            return reference;
        }
        if fs::metadata(&path).is_ok_and(|metadata| metadata.len() > 1024 * 1024) {
            let rotated = path.with_extension("log.1");
            let _ = fs::remove_file(&rotated);
            let _ = fs::rename(&path, rotated);
        }
        if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_millis())
                .unwrap_or_default();
            let record = json!({
                "reference": reference,
                "timestampMs": timestamp,
                "operation": operation,
                "outcome": outcome,
                "durationMs": started.elapsed().as_millis(),
                "details": details,
            });
            let _ = writeln!(file, "{record}");
        }
        reference
    }

    fn correlate_failure_details(
        &self,
        operation: &str,
        outcome: &str,
        mut failure: WorkbenchFailure,
        details: Value,
    ) -> WorkbenchFailure {
        if failure.log_reference.is_none() {
            failure.log_reference =
                Some(self.log_event_timed(operation, outcome, Instant::now(), details));
        }
        failure
    }

    /// Waits for a Workbench process that was not running before the launch.
    fn wait_for_new_workbench_process(
        &self,
        before: &[ProcessIdentity],
        deadline: Duration,
    ) -> Option<ProcessIdentity> {
        let started = Instant::now();
        loop {
            if let Some(process) = process::workbench_processes()
                .into_iter()
                .find(|process| !before.contains(process))
            {
                return Some(process);
            }
            if started.elapsed() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    fn wait_for_net_api(&self, deadline: Duration) -> bool {
        let started = std::time::Instant::now();
        while started.elapsed() < deadline {
            if self.gateway.status().is_ok() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        false
    }

    fn observe_processes(&self, processes: &[ProcessIdentity]) {
        if let Ok(mut observed) = self.observed_processes.lock() {
            observed.extend(processes.iter().copied());
        }
    }
}

fn entity_mutation_operation(api_func: &str) -> &str {
    match api_func {
        "RST_WorkbenchCreateEntity" => "create-entity",
        "RST_WorkbenchRenameEntity" => "rename-entity",
        "RST_WorkbenchMoveEntity" => "move-entity",
        "RST_WorkbenchRotateEntity" => "rotate-entity",
        "RST_WorkbenchReparentEntity" => "reparent-entity",
        "RST_WorkbenchDuplicateEntity" => "duplicate-entity",
        "RST_WorkbenchDeleteEntity" => "delete-entity",
        "RST_WorkbenchSetEntityProperty" => "set-entity-property",
        "RST_WorkbenchCreatePrefab" => "create-prefab",
        "RST_WorkbenchSavePrefab" => "save-prefab",
        "RST_WorkbenchSetPrefabProperty" => "set-prefab-property",
        _ => "entity-mutation",
    }
}

fn entity_mutation_audit_details(request: &Value, result: &WorkbenchEntityMutationResult) -> Value {
    let entity = result.entity.as_ref();
    json!({
        "entityId": request.get("entityId").and_then(Value::as_str),
        "parentEntityId": request.get("parentEntityId").and_then(Value::as_str),
        "propertyName": request.get("propertyName").and_then(Value::as_str),
        "resultEntityId": entity.map(|entity| entity.entity_id.as_str()),
        "resultClass": entity.map(|entity| entity.class_name.as_str()),
        "activeLayerId": result.active_layer_id,
    })
}

fn component_mutation_operation(api_func: &str) -> Option<&str> {
    match api_func {
        "RST_WorkbenchAddComponent" => Some("add-component"),
        "RST_WorkbenchRemoveComponent" => Some("remove-component"),
        "RST_WorkbenchSetComponentProperty" => Some("set-component-property"),
        "RST_WorkbenchSetPrefabComponentProperty" => Some("set-prefab-component-property"),
        _ => None,
    }
}

fn component_mutation_audit_details(request: &Value, result: &WorkbenchComponentResult) -> Value {
    let entity = result.entity.as_ref();
    json!({
        "entityId": request.get("entityId").and_then(Value::as_str),
        "componentClass": request.get("className").and_then(Value::as_str),
        "propertyName": request.get("propertyName").and_then(Value::as_str),
        "resultEntityId": entity.map(|entity| entity.entity_id.as_str()),
        "resultClass": entity.map(|entity| entity.class_name.as_str()),
        "componentCount": result.components.len(),
    })
}

impl WorkbenchGateway {
    pub fn new(options: WorkbenchGatewayOptions) -> Self {
        Self::with_host(options, workbench_host().clone())
    }

    fn with_host(options: WorkbenchGatewayOptions, host: WorkbenchHost) -> Self {
        Self {
            options,
            host,
            request_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn status(&self) -> Result<WorkbenchStatus, WorkbenchFailure> {
        self.status_with_timing().map(|(status, _)| status)
    }

    pub fn status_with_timing(
        &self,
    ) -> Result<(WorkbenchStatus, WorkbenchRequestTiming), WorkbenchFailure> {
        let (value, timing) = self.request_with_timing(
            json!({"APIFunc": "IsWorkbenchRunning"}),
            self.options.status_deadline,
        )?;
        serde_json::from_value::<RawWorkbenchStatus>(value)
            .map(|value| WorkbenchStatus {
                is_running: value.is_running,
                scripts_compiled: value.scripts_compiled,
            })
            .map(|status| (status, timing))
            .map_err(|_| failure(WorkbenchFailureCode::Protocol))
    }

    pub fn validate_scripts(&self) -> Result<WorkbenchValidation, WorkbenchFailure> {
        let value = self.request(
            json!({"APIFunc": "ValidateScripts", "Configuration": "WORKBENCH"}),
            self.options.validation_deadline,
        )?;
        let value = serde_json::from_value::<RawValidation>(value)
            .map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        let mut diagnostics: Vec<WorkbenchCompilerDiagnostic> = Vec::new();
        let mut identities = HashMap::<DiagnosticIdentity, usize>::new();
        for (severity, raw) in value
            .errors
            .into_iter()
            .map(|value| (WorkbenchDiagnosticSeverity::Error, value))
            .chain(
                value
                    .warnings
                    .into_iter()
                    .map(|value| (WorkbenchDiagnosticSeverity::Warning, value)),
            )
        {
            let diagnostic = WorkbenchCompilerDiagnostic {
                severity,
                message: raw.error,
                location: WorkbenchDiagnosticLocation {
                    file: raw.file,
                    file_abs: raw
                        .file_abs
                        .as_deref()
                        .and_then(|value| self.host.to_host_path(value)),
                    addon: raw.addon,
                    line: raw.line,
                },
            };
            let identity = DiagnosticIdentity::from(&diagnostic);
            if let Some(index) = identities.get(&identity).copied() {
                if diagnostics[index].severity == WorkbenchDiagnosticSeverity::Warning
                    && severity == WorkbenchDiagnosticSeverity::Error
                {
                    diagnostics[index] = diagnostic;
                }
            } else {
                identities.insert(identity, diagnostics.len());
                diagnostics.push(diagnostic);
            }
        }
        Ok(WorkbenchValidation {
            profile: "WORKBENCH".to_string(),
            success: value.success,
            diagnostics,
        })
    }

    fn request(&self, payload: Value, deadline: Duration) -> Result<Value, WorkbenchFailure> {
        self.request_with_timing(payload, deadline)
            .map(|(value, _)| value)
    }

    fn request_with_timing(
        &self,
        payload: Value,
        deadline: Duration,
    ) -> Result<(Value, WorkbenchRequestTiming), WorkbenchFailure> {
        let started = Instant::now();
        let lock_started = Instant::now();
        let _request = self
            .request_lock
            .lock()
            .map_err(|_| failure(WorkbenchFailureCode::Unavailable))?;
        let lock_wait = lock_started.elapsed();
        let ip = self
            .options
            .host
            .parse::<IpAddr>()
            .map_err(|_| failure(WorkbenchFailureCode::Unavailable))?;
        if !ip.is_loopback() {
            return Err(failure(WorkbenchFailureCode::Unavailable));
        }
        let address = SocketAddr::new(ip, self.options.port);
        let connect_started = Instant::now();
        let mut stream = TcpStream::connect_timeout(&address, deadline)
            .map_err(|_| failure(WorkbenchFailureCode::Unavailable))?;
        let connect = connect_started.elapsed();
        stream
            .set_read_timeout(Some(deadline))
            .map_err(|_| failure(WorkbenchFailureCode::Unavailable))?;
        stream
            .set_write_timeout(Some(deadline))
            .map_err(|_| failure(WorkbenchFailureCode::Unavailable))?;
        let write_started = Instant::now();
        stream
            .write_all(&1_i32.to_le_bytes())
            .and_then(|_| write_string(&mut stream, "ReforgerScriptTools"))
            .and_then(|_| write_string(&mut stream, "JsonRPC"))
            .and_then(|_| write_string(&mut stream, &payload.to_string()))
            .map_err(map_io_failure)?;
        stream.shutdown(Shutdown::Write).map_err(map_io_failure)?;
        let write = write_started.elapsed();
        let response_header_started = Instant::now();
        let error_code = read_string(&mut stream).map_err(|error| {
            if started.elapsed() >= deadline {
                failure(WorkbenchFailureCode::Timeout)
            } else {
                map_io_failure(error)
            }
        })?;
        let response_header = response_header_started.elapsed();
        if started.elapsed() >= deadline {
            return Err(failure(WorkbenchFailureCode::Timeout));
        }
        let response_body_started = Instant::now();
        let payload = read_string(&mut stream).map_err(|error| {
            if started.elapsed() >= deadline {
                failure(WorkbenchFailureCode::Timeout)
            } else {
                map_io_failure(error)
            }
        })?;
        let response_body = response_body_started.elapsed();
        if started.elapsed() >= deadline {
            return Err(failure(WorkbenchFailureCode::Timeout));
        }
        if error_code != "Ok" {
            return Err(failure(WorkbenchFailureCode::WorkbenchError));
        }
        let decode_started = Instant::now();
        let value =
            serde_json::from_str(&payload).map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        let decode = decode_started.elapsed();
        Ok((
            value,
            WorkbenchRequestTiming {
                lock_wait_ms: duration_ms(lock_wait),
                connect_ms: duration_ms(connect),
                write_ms: duration_ms(write),
                response_header_ms: duration_ms(response_header),
                response_body_ms: duration_ms(response_body),
                decode_ms: duration_ms(decode),
                total_ms: duration_ms(started.elapsed()),
            },
        ))
    }
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Deserialize)]
struct RawWorkbenchStatus {
    #[serde(rename = "IsRunning")]
    is_running: bool,
    #[serde(rename = "ScriptsCompiled")]
    scripts_compiled: bool,
}

#[derive(Deserialize)]
struct RawValidation {
    #[serde(rename = "Success")]
    success: bool,
    #[serde(rename = "Errors")]
    errors: Vec<RawDiagnostic>,
    #[serde(rename = "Warnings")]
    warnings: Vec<RawDiagnostic>,
}

#[derive(Deserialize)]
struct RawDiagnostic {
    error: String,
    file: String,
    #[serde(rename = "fileAbs")]
    file_abs: Option<String>,
    addon: Option<String>,
    line: usize,
}

#[derive(Debug, Hash, PartialEq, Eq)]
struct DiagnosticIdentity {
    message: String,
    file: String,
    file_abs: Option<PathBuf>,
    addon: Option<String>,
    line: usize,
}

impl From<&WorkbenchCompilerDiagnostic> for DiagnosticIdentity {
    fn from(value: &WorkbenchCompilerDiagnostic) -> Self {
        Self {
            message: value.message.clone(),
            file: value.location.file.clone(),
            file_abs: value.location.file_abs.clone(),
            addon: value.location.addon.clone(),
            line: value.location.line,
        }
    }
}

fn write_string(stream: &mut impl Write, value: &str) -> std::io::Result<()> {
    let bytes = value.as_bytes();
    let length = i32::try_from(bytes.len())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "string too large"))?;
    stream.write_all(&length.to_le_bytes())?;
    stream.write_all(bytes)
}

fn read_string(stream: &mut impl Read) -> std::io::Result<String> {
    let mut length = [0_u8; 4];
    stream.read_exact(&mut length)?;
    let length = i32::from_le_bytes(length);
    if length < 0 || length as usize > MAX_RESPONSE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid NET API string length",
        ));
    }
    let mut bytes = vec![0_u8; length as usize];
    stream.read_exact(&mut bytes)?;
    String::from_utf8(bytes)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid UTF-8"))
}

fn map_io_failure(error: std::io::Error) -> WorkbenchFailure {
    if matches!(
        error.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    ) {
        failure(WorkbenchFailureCode::Timeout)
    } else {
        failure(WorkbenchFailureCode::Protocol)
    }
}

fn failure(code: WorkbenchFailureCode) -> WorkbenchFailure {
    WorkbenchFailure {
        code,
        log_reference: None,
    }
}

static LOG_REFERENCE_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static DELETE_CONFIRMATION_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static PROPERTY_DESCRIPTOR_SEQUENCE: AtomicU64 = AtomicU64::new(1);

fn next_log_reference() -> String {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis())
        .unwrap_or_default();
    let sequence = LOG_REFERENCE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("wb-{}-{timestamp}-{sequence}", std::process::id())
}

fn failure_code(code: WorkbenchFailureCode) -> &'static str {
    match code {
        WorkbenchFailureCode::ConsentRequired => "workbench_installation_consent_required",
        WorkbenchFailureCode::Unavailable => "workbench_unavailable",
        WorkbenchFailureCode::Timeout => "workbench_timeout",
        WorkbenchFailureCode::Protocol => "workbench_protocol_error",
        WorkbenchFailureCode::WorkbenchError => "workbench_error",
        WorkbenchFailureCode::CaptureUnavailable => "workbench_capture_unavailable",
        WorkbenchFailureCode::CaptureInvalidRegion => "workbench_capture_invalid_region",
        WorkbenchFailureCode::CaptureTooLarge => "workbench_screenshot_too_large",
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BridgeManifest {
    bridge_version: String,
    protocol_version: u32,
    files: Vec<BridgeManifestFile>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BridgeManifestFile {
    name: String,
    sha256: String,
}

#[derive(Deserialize)]
struct RawBridgeCapabilities {
    #[serde(rename = "bridgeVersion")]
    bridge_version: String,
    #[serde(rename = "protocolVersion")]
    protocol_version: u32,
    #[serde(rename = "runtimeGeneration")]
    runtime_generation: i32,
    capabilities: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawWorkbenchEditorList {
    editors: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawWorkbenchOpenEditor {
    editor_id: String,
    opened: Value,
    status: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawWorkbenchOpenResource {
    resource_path: String,
    opened: Value,
    status: String,
}

#[derive(Deserialize)]
struct RawBridgeReloadAction {
    #[serde(rename = "bridgeVersion")]
    _bridge_version: String,
    #[serde(rename = "protocolVersion")]
    protocol_version: u32,
    #[serde(rename = "reloadActionAccepted")]
    _accepted: Value,
    #[serde(rename = "reloadActionPath")]
    action_path: String,
}

fn reload_action_path(
    dispatch: Result<RawBridgeReloadAction, WorkbenchFailure>,
) -> Result<String, WorkbenchFailure> {
    match dispatch {
        // A false dispatcher acknowledgement is not a reliable failure signal: Workbench can
        // begin reloading while its handler still reports false. The result remains a dispatch
        // observation; a changed typed runtime generation is required for success.
        Ok(action) => Ok(action.action_path),
        // Reload can tear down the in-flight handler before it returns. Continue only because
        // the caller will require a changed typed runtime generation before returning success.
        Err(dispatch_failure) if dispatch_failure.code == WorkbenchFailureCode::Timeout => {
            Ok("Plugins/Settings/Reload WB Scripts".to_string())
        }
        Err(dispatch_failure) => Err(dispatch_failure),
    }
}

#[derive(Deserialize)]
struct RawBridgeSaveAllAction {
    #[serde(rename = "bridgeVersion")]
    _bridge_version: String,
    #[serde(rename = "protocolVersion")]
    protocol_version: u32,
    #[serde(rename = "saveAllActionAccepted")]
    accepted: Value,
    #[serde(rename = "worldSaveActionAccepted")]
    world_save_accepted: Value,
    #[serde(rename = "worldSaveStatus")]
    world_save_status: String,
    #[serde(rename = "saveAllActionPath")]
    action_path: String,
}

#[derive(Deserialize)]
struct RawBridgeState {
    #[serde(rename = "bridgeVersion")]
    bridge_version: String,
    #[serde(rename = "protocolVersion")]
    protocol_version: u32,
    mode: String,
    #[serde(rename = "activeWorldPath")]
    active_world_path: Option<String>,
    #[serde(rename = "worldEditorActive", default)]
    world_editor_active: Value,
    #[serde(rename = "worldEditorModulePresent", default)]
    world_editor_module_present: Value,
    #[serde(rename = "worldEditorApiAvailable", default)]
    world_editor_api_available: Value,
    #[serde(rename = "playSession")]
    play_session: Option<String>,
    #[serde(rename = "loadedAddons")]
    loaded_addons: String,
    #[serde(rename = "currentSubScene")]
    current_sub_scene: Option<i32>,
    #[serde(rename = "currentEntityLayerId")]
    current_entity_layer_id: Option<i32>,
    #[serde(rename = "activeSubsceneLayer")]
    active_subscene_layer: Option<String>,
}

#[derive(Deserialize)]
struct RawBridgeProjectContext {
    #[serde(rename = "bridgeVersion")]
    bridge_version: String,
    #[serde(rename = "protocolVersion")]
    protocol_version: u32,
    #[serde(rename = "loadedAddons", default)]
    loaded_addons: String,
}

#[derive(Deserialize)]
struct RawWorkbenchLoadedAddonGraph {
    #[serde(rename = "bridgeVersion")]
    bridge_version: String,
    #[serde(rename = "protocolVersion")]
    protocol_version: u32,
    #[serde(rename = "currentProjectFile", default)]
    current_project_file: String,
    #[serde(rename = "graphJson")]
    graph_json: String,
}

#[derive(Deserialize)]
struct RawBridgeWorldSelection {
    #[serde(rename = "bridgeVersion")]
    bridge_version: String,
    #[serde(rename = "protocolVersion")]
    protocol_version: u32,
    #[serde(rename = "editorAvailable")]
    editor_available: Value,
    status: String,
    #[serde(rename = "selectedCount")]
    selected_count: u32,
    #[serde(rename = "selectedEntities", default)]
    selected_entities: String,
    #[serde(rename = "selectedEntitiesTruncated", default)]
    selected_entities_truncated: Value,
}

#[derive(Deserialize)]
struct RawBridgeResourceList {
    #[serde(rename = "bridgeVersion")]
    bridge_version: String,
    #[serde(rename = "protocolVersion")]
    protocol_version: u32,
    #[serde(rename = "loadedAddons", default)]
    loaded_addons: String,
    #[serde(rename = "resources", default)]
    resources: String,
    #[serde(rename = "resourceDetails", default)]
    resource_details: String,
    #[serde(rename = "hasMore", default)]
    has_more: Value,
}

#[derive(Deserialize)]
struct RawBridgeResourceInspection {
    found: Value,
    status: String,
    #[serde(rename = "resourceName")]
    resource_name: Option<String>,
    #[serde(rename = "className")]
    class_name: Option<String>,
    #[serde(rename = "sourceAddons", default)]
    source_addons: String,
    #[serde(rename = "sourceAddonsTruncated", default)]
    source_addons_truncated: Value,
}

#[derive(Deserialize)]
struct RawBridgePlaySessionResult {
    accepted: Value,
    status: String,
}

#[derive(Deserialize)]
struct RawBridgeSelectedEntityHierarchy {
    #[serde(rename = "bridgeVersion")]
    bridge_version: String,
    #[serde(rename = "protocolVersion")]
    protocol_version: u32,
    #[serde(rename = "editorAvailable")]
    editor_available: Value,
    status: String,
    #[serde(default)]
    entity: String,
    #[serde(rename = "resourceName")]
    resource_name: Option<String>,
    #[serde(rename = "resourceReferenceKind", default)]
    resource_reference_kind: String,
    #[serde(rename = "contributorAddons", default)]
    contributor_addons: String,
    #[serde(rename = "contributorAddonsTruncated", default)]
    contributor_addons_truncated: Value,
    #[serde(default)]
    ancestors: String,
    #[serde(rename = "ancestorsTruncated", default)]
    ancestors_truncated: Value,
    #[serde(default)]
    children: String,
    #[serde(rename = "childrenTruncated", default)]
    children_truncated: Value,
    #[serde(default)]
    components: String,
    #[serde(rename = "componentProperties", default)]
    component_properties: String,
    #[serde(rename = "componentPropertiesTruncated", default)]
    component_properties_truncated: Value,
    #[serde(default)]
    properties: String,
    #[serde(rename = "propertiesTruncated", default)]
    properties_truncated: Value,
}

#[derive(Deserialize)]
struct RawBridgeEntityList {
    #[serde(rename = "bridgeVersion")]
    bridge_version: String,
    #[serde(rename = "protocolVersion")]
    protocol_version: u32,
    #[serde(rename = "worldPath", default)]
    world_path: String,
    #[serde(default)]
    entities: String,
    #[serde(rename = "hasMore", default)]
    has_more: Value,
}

#[derive(Deserialize)]
struct RawBridgeEntitySearch {
    #[serde(rename = "bridgeVersion")]
    bridge_version: String,
    #[serde(rename = "protocolVersion")]
    protocol_version: u32,
    #[serde(rename = "worldPath", default)]
    world_path: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    results: String,
    #[serde(rename = "totalMatches", default)]
    total_matches: u32,
    #[serde(rename = "namedMatches", default)]
    named_matches: u32,
    #[serde(rename = "hasMore", default)]
    has_more: Value,
    #[serde(rename = "relationTraversalTruncated", default)]
    relation_traversal_truncated: Value,
}

#[derive(Deserialize)]
struct RawBridgeLayerState {
    #[serde(rename = "bridgeVersion")]
    bridge_version: String,
    #[serde(rename = "protocolVersion")]
    protocol_version: u32,
    status: String,
    #[serde(rename = "subScene")]
    sub_scene: i32,
    #[serde(rename = "layerId")]
    layer_id: i32,
    #[serde(rename = "layerPath", default)]
    layer_path: String,
    #[serde(default)]
    visible: Value,
    #[serde(rename = "explicitlyLocked", default)]
    explicitly_locked: Value,
    #[serde(rename = "lockedInHierarchy", default)]
    locked_in_hierarchy: Value,
}

#[derive(Deserialize)]
struct RawBridgeEntitySelection {
    #[serde(rename = "bridgeVersion")]
    bridge_version: String,
    #[serde(rename = "protocolVersion")]
    protocol_version: u32,
    status: String,
    #[serde(rename = "activeLayerId", default)]
    active_layer_id: Option<i32>,
    #[serde(default)]
    entity: String,
    #[serde(default)]
    destination: String,
    #[serde(
        rename = "destinationExists",
        default,
        deserialize_with = "deserialize_optional_boolish"
    )]
    destination_exists: Option<bool>,
}

#[derive(Deserialize)]
struct RawBridgeShapePoints {
    #[serde(rename = "bridgeVersion")]
    bridge_version: String,
    #[serde(rename = "protocolVersion")]
    protocol_version: u32,
    status: String,
    #[serde(default)]
    entity: String,
    #[serde(rename = "shapeClass", default)]
    shape_class: String,
    #[serde(default, deserialize_with = "deserialize_boolish")]
    closed: bool,
    #[serde(default)]
    points: String,
}

#[derive(Deserialize)]
struct RawBridgeShapeGeometry {
    #[serde(rename = "bridgeVersion")]
    bridge_version: String,
    #[serde(rename = "protocolVersion")]
    protocol_version: u32,
    status: String,
    #[serde(default)]
    entity: String,
    #[serde(rename = "shapeClass", default)]
    shape_class: String,
    #[serde(default, deserialize_with = "deserialize_boolish")]
    closed: bool,
    #[serde(default)]
    points: String,
    #[serde(rename = "fromSpace", default)]
    from_space: String,
    #[serde(rename = "toSpace", default)]
    to_space: String,
    #[serde(rename = "spacingMeters", default)]
    spacing_meters: f32,
    #[serde(rename = "originalPointCount", default)]
    original_point_count: usize,
    #[serde(rename = "resultPointCount", default)]
    result_point_count: usize,
    #[serde(rename = "pathLength", default)]
    path_length: f32,
    #[serde(rename = "skippedZeroLengthSegments", default)]
    skipped_zero_length_segments: usize,
}

impl RawBridgeShapeGeometry {
    fn validate(self) -> Result<Self, WorkbenchFailure> {
        parse_optional_world_selection_record(&self.entity)
            .map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        parse_shape_points(&self.points).ok_or_else(|| failure(WorkbenchFailureCode::Protocol))?;
        Ok(self)
    }
    fn entity(&self) -> Result<Option<WorkbenchSelectedEntity>, WorkbenchFailure> {
        parse_optional_world_selection_record(&self.entity)
            .map_err(|_| failure(WorkbenchFailureCode::Protocol))
    }
    fn points(&self) -> Result<Vec<WorkbenchEntityPosition>, WorkbenchFailure> {
        parse_shape_points(&self.points).ok_or_else(|| failure(WorkbenchFailureCode::Protocol))
    }
    fn into_shape_points(self) -> WorkbenchShapePoints {
        let entity = self.entity().expect("validated shape geometry entity");
        let points = self.points().expect("validated shape geometry points");
        WorkbenchShapePoints {
            bridge_version: self.bridge_version,
            protocol_version: self.protocol_version,
            status: self.status,
            entity,
            shape_class: (!self.shape_class.is_empty()).then_some(self.shape_class),
            closed: self.closed,
            points,
        }
    }
    fn into_conversion(self) -> WorkbenchShapePointConversion {
        let entity = self.entity().expect("validated shape geometry entity");
        let points = self.points().expect("validated shape geometry points");
        WorkbenchShapePointConversion {
            bridge_version: self.bridge_version,
            protocol_version: self.protocol_version,
            status: self.status,
            entity,
            shape_class: (!self.shape_class.is_empty()).then_some(self.shape_class),
            from_space: self.from_space,
            to_space: self.to_space,
            points,
        }
    }
    fn into_resample(self) -> WorkbenchPolylineResample {
        let entity = self.entity().expect("validated shape geometry entity");
        let points = self.points().expect("validated shape geometry points");
        WorkbenchPolylineResample {
            bridge_version: self.bridge_version,
            protocol_version: self.protocol_version,
            status: self.status,
            entity,
            shape_class: (!self.shape_class.is_empty()).then_some(self.shape_class),
            closed: self.closed,
            points,
            spacing_meters: self.spacing_meters,
            original_point_count: self.original_point_count,
            result_point_count: self.result_point_count,
            path_length: self.path_length,
            skipped_zero_length_segments: self.skipped_zero_length_segments,
        }
    }
}

#[derive(Deserialize)]
struct RawBridgePrefabResourceComponentMutation {
    #[serde(rename = "bridgeVersion")]
    bridge_version: String,
    #[serde(rename = "protocolVersion")]
    protocol_version: u32,
    status: String,
    #[serde(rename = "resourceName")]
    resource_name: String,
    #[serde(rename = "componentIndex", default)]
    component_index: i32,
    #[serde(rename = "componentClass", default)]
    component_class: String,
    #[serde(
        rename = "templateSaved",
        default,
        deserialize_with = "deserialize_boolish"
    )]
    template_saved: bool,
}

#[derive(Deserialize)]
struct RawBridgePrefabResourcePropertyMutation {
    #[serde(rename = "bridgeVersion")]
    bridge_version: String,
    #[serde(rename = "protocolVersion")]
    protocol_version: u32,
    status: String,
    #[serde(rename = "resourceName")]
    resource_name: String,
    #[serde(
        rename = "templateSaved",
        default,
        deserialize_with = "deserialize_boolish"
    )]
    template_saved: bool,
}

fn deserialize_boolish<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_optional_boolish(deserializer)?
        .ok_or_else(|| serde::de::Error::custom("expected templateSaved to be a boolean or 0/1"))
}

fn deserialize_optional_boolish<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(value)),
        Some(Value::Number(value)) => match value.as_i64() {
            Some(0) => Ok(Some(false)),
            Some(1) => Ok(Some(true)),
            _ => Err(serde::de::Error::custom(
                "expected destinationExists to be a boolean or 0/1",
            )),
        },
        Some(Value::String(value)) => match value.as_str() {
            "0" | "false" => Ok(Some(false)),
            "1" | "true" => Ok(Some(true)),
            _ => Err(serde::de::Error::custom(
                "expected destinationExists to be a boolean or 0/1",
            )),
        },
        Some(_) => Err(serde::de::Error::custom(
            "expected destinationExists to be a boolean or 0/1",
        )),
    }
}

#[derive(Deserialize)]
struct RawBridgeEntityRadiusQuery {
    #[serde(rename = "bridgeVersion")]
    bridge_version: String,
    #[serde(rename = "protocolVersion")]
    protocol_version: u32,
    status: String,
    #[serde(rename = "centerX")]
    center_x: f32,
    #[serde(rename = "centerY")]
    center_y: f32,
    #[serde(rename = "centerZ")]
    center_z: f32,
    #[serde(rename = "radiusMeters")]
    radius_meters: f32,
    #[serde(rename = "queryScope")]
    query_scope: String,
    #[serde(rename = "requireObject")]
    require_object: Value,
    #[serde(rename = "excludeProxies")]
    exclude_proxies: Value,
    #[serde(default)]
    entities: String,
    #[serde(default)]
    truncated: Value,
}

#[derive(Deserialize)]
struct RawBridgeTerrainSample {
    #[serde(rename = "bridgeVersion")]
    bridge_version: String,
    #[serde(rename = "protocolVersion")]
    protocol_version: u32,
    status: String,
    #[serde(rename = "halfExtentMeters", default)]
    half_extent_meters: f32,
    #[serde(rename = "requestedSpacingMeters", default)]
    requested_spacing_meters: f32,
    #[serde(rename = "effectiveSpacingMeters", default)]
    effective_spacing_meters: f32,
    #[serde(rename = "spacingClamped", default)]
    spacing_clamped: Value,
    #[serde(rename = "gridOriginX", default)]
    grid_origin_x: f32,
    #[serde(rename = "gridOriginZ", default)]
    grid_origin_z: f32,
    #[serde(rename = "gridWidth", default)]
    grid_width: u32,
    #[serde(rename = "gridHeight", default)]
    grid_height: u32,
    #[serde(default)]
    heights: String,
    #[serde(rename = "waterTypes", default)]
    water_types: String,
    #[serde(rename = "waterSurfaceHeights", default)]
    water_surface_heights: String,
    #[serde(rename = "waterDepthsAboveTerrain", default)]
    water_depths_above_terrain: String,
    #[serde(rename = "boundsMinX", default)]
    bounds_min_x: f32,
    #[serde(rename = "boundsMinY", default)]
    bounds_min_y: f32,
    #[serde(rename = "boundsMinZ", default)]
    bounds_min_z: f32,
    #[serde(rename = "boundsMaxX", default)]
    bounds_max_x: f32,
    #[serde(rename = "boundsMaxY", default)]
    bounds_max_y: f32,
    #[serde(rename = "boundsMaxZ", default)]
    bounds_max_z: f32,
    #[serde(rename = "heightmapResolutionX", default)]
    heightmap_resolution_x: u32,
    #[serde(rename = "heightmapResolutionZ", default)]
    heightmap_resolution_z: u32,
    #[serde(rename = "nativeSpacingMeters", default)]
    native_spacing_meters: f32,
    #[serde(rename = "tileCountX", default)]
    tile_count_x: u32,
    #[serde(rename = "tileCountZ", default)]
    tile_count_z: u32,
}

#[derive(Deserialize)]
struct RawBridgeViewportContext {
    #[serde(rename = "bridgeVersion")]
    bridge_version: String,
    #[serde(rename = "protocolVersion")]
    protocol_version: u32,
    status: String,
    #[serde(default)]
    width: i32,
    #[serde(default)]
    height: i32,
    #[serde(rename = "mouseX", default)]
    mouse_x: i32,
    #[serde(rename = "mouseY", default)]
    mouse_y: i32,
    #[serde(rename = "mouseInside", default)]
    mouse_inside: Value,
    #[serde(rename = "cameraX", default)]
    camera_x: f32,
    #[serde(rename = "cameraY", default)]
    camera_y: f32,
    #[serde(rename = "cameraZ", default)]
    camera_z: f32,
    #[serde(rename = "cameraDirectionX", default)]
    camera_direction_x: f32,
    #[serde(rename = "cameraDirectionY", default)]
    camera_direction_y: f32,
    #[serde(rename = "cameraDirectionZ", default)]
    camera_direction_z: f32,
    #[serde(rename = "startX", default)]
    start_x: f32,
    #[serde(rename = "startY", default)]
    start_y: f32,
    #[serde(rename = "startZ", default)]
    start_z: f32,
    #[serde(rename = "endX", default)]
    end_x: f32,
    #[serde(rename = "endY", default)]
    end_y: f32,
    #[serde(rename = "endZ", default)]
    end_z: f32,
    #[serde(rename = "directionX", default)]
    direction_x: f32,
    #[serde(rename = "directionY", default)]
    direction_y: f32,
    #[serde(rename = "directionZ", default)]
    direction_z: f32,
}
#[derive(Deserialize)]
struct RawBridgeTrace {
    #[serde(rename = "bridgeVersion")]
    bridge_version: String,
    #[serde(rename = "protocolVersion")]
    protocol_version: u32,
    status: String,
    #[serde(default)]
    hit: Value,
    #[serde(default)]
    fraction: f32,
    #[serde(default)]
    distance: f32,
    #[serde(rename = "hitX", default)]
    hit_x: f32,
    #[serde(rename = "hitY", default)]
    hit_y: f32,
    #[serde(rename = "hitZ", default)]
    hit_z: f32,
    #[serde(rename = "normalX", default)]
    normal_x: f32,
    #[serde(rename = "normalY", default)]
    normal_y: f32,
    #[serde(rename = "normalZ", default)]
    normal_z: f32,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    entity: String,
    #[serde(rename = "colliderName", default)]
    collider_name: String,
    #[serde(default)]
    material: String,
}

#[derive(Deserialize)]
struct RawBridgeComponentResult {
    #[serde(rename = "bridgeVersion")]
    bridge_version: String,
    #[serde(rename = "protocolVersion")]
    protocol_version: u32,
    status: String,
    #[serde(default)]
    entity: String,
    #[serde(default)]
    components: String,
    #[serde(default)]
    properties: String,
}
#[derive(Deserialize)]
struct RawBridgePropertyList {
    #[serde(rename = "bridgeVersion")]
    bridge_version: String,
    #[serde(rename = "protocolVersion")]
    protocol_version: u32,
    status: String,
    #[serde(default)]
    properties: String,
}

#[derive(Deserialize)]
struct RawBridgePrefabContext {
    #[serde(rename = "bridgeVersion")]
    bridge_version: String,
    #[serde(rename = "protocolVersion")]
    protocol_version: u32,
    status: String,
    #[serde(default)]
    entity: String,
    #[serde(rename = "memberId", default)]
    member_id: String,
    #[serde(rename = "resourceName", default)]
    resource_name: String,
    #[serde(rename = "resourceReferenceKind", default)]
    resource_reference_kind: String,
    #[serde(rename = "contributorAddons", default)]
    contributor_addons: String,
    #[serde(rename = "ancestorResources", default)]
    ancestor_resources: String,
    #[serde(rename = "ancestorResourcesTruncated", default)]
    ancestor_resources_truncated: Value,
    #[serde(rename = "prefabEditMode", default)]
    prefab_edit_mode: Value,
    #[serde(default)]
    components: String,
    #[serde(rename = "componentProperties", default)]
    component_properties: String,
    #[serde(default)]
    children: String,
    #[serde(rename = "childrenTruncated", default)]
    children_truncated: Value,
    #[serde(default)]
    properties: String,
    #[serde(rename = "propertiesTruncated", default)]
    properties_truncated: Value,
    #[serde(rename = "childCount", default)]
    child_count: u32,
}

fn workbench_bool(value: &Value) -> bool {
    matches!(value, Value::Bool(true)) || value.as_i64().is_some_and(|integer| integer != 0)
}

fn parse_terrain_metadata(raw: &RawBridgeTerrainSample) -> Result<WorkbenchTerrainMetadata, ()> {
    if !raw.bounds_min_x.is_finite()
        || !raw.bounds_min_y.is_finite()
        || !raw.bounds_min_z.is_finite()
        || !raw.bounds_max_x.is_finite()
        || !raw.bounds_max_y.is_finite()
        || !raw.bounds_max_z.is_finite()
        || raw.bounds_min_x > raw.bounds_max_x
        || raw.bounds_min_y > raw.bounds_max_y
        || raw.bounds_min_z > raw.bounds_max_z
        || raw.heightmap_resolution_x == 0
        || raw.heightmap_resolution_z == 0
        || !raw.native_spacing_meters.is_finite()
        || raw.native_spacing_meters <= 0.0
        || raw.tile_count_x == 0
        || raw.tile_count_z == 0
    {
        return Err(());
    }
    Ok(WorkbenchTerrainMetadata {
        bounds: WorkbenchTerrainBounds {
            min: WorkbenchEntityPosition {
                x: raw.bounds_min_x,
                y: raw.bounds_min_y,
                z: raw.bounds_min_z,
            },
            max: WorkbenchEntityPosition {
                x: raw.bounds_max_x,
                y: raw.bounds_max_y,
                z: raw.bounds_max_z,
            },
        },
        heightmap_resolution_x: raw.heightmap_resolution_x,
        heightmap_resolution_z: raw.heightmap_resolution_z,
        native_spacing_meters: raw.native_spacing_meters,
        tile_count_x: raw.tile_count_x,
        tile_count_z: raw.tile_count_z,
    })
}

fn parse_terrain_grid(
    raw: &RawBridgeTerrainSample,
    requested_spacing_meters: Option<f32>,
    max_samples: usize,
) -> Result<WorkbenchTerrainGrid, ()> {
    let sample_count = (raw.grid_width as usize).saturating_mul(raw.grid_height as usize);
    if raw.grid_width == 0
        || raw.grid_height == 0
        || sample_count > max_samples
        || !raw.half_extent_meters.is_finite()
        || raw.half_extent_meters <= 0.0
        || !raw.requested_spacing_meters.is_finite()
        || !raw.effective_spacing_meters.is_finite()
        || raw.effective_spacing_meters <= 0.0
        || !raw.grid_origin_x.is_finite()
        || !raw.grid_origin_z.is_finite()
    {
        return Err(());
    }
    let heights = raw
        .heights
        .split(';')
        .map(|value| {
            if value == "~" {
                Ok(None)
            } else {
                value
                    .parse::<f32>()
                    .ok()
                    .filter(|height| height.is_finite())
                    .map(Some)
                    .ok_or(())
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    if heights.len() != sample_count {
        return Err(());
    }
    Ok(WorkbenchTerrainGrid {
        origin: WorkbenchTerrainCoordinate {
            x: raw.grid_origin_x,
            z: raw.grid_origin_z,
        },
        requested_half_extent_meters: raw.half_extent_meters,
        requested_spacing_meters,
        effective_spacing_meters: raw.effective_spacing_meters,
        spacing_clamped: workbench_bool(&raw.spacing_clamped),
        width: raw.grid_width,
        height: raw.grid_height,
        heights,
    })
}

fn summarize_terrain_grid(grid: &WorkbenchTerrainGrid) -> WorkbenchTerrainSummary {
    let valid_heights = grid.heights.iter().flatten().copied().collect::<Vec<_>>();
    let valid_sample_count = valid_heights.len() as u32;
    let minimum_height = valid_heights.iter().copied().reduce(f32::min);
    let maximum_height = valid_heights.iter().copied().reduce(f32::max);
    let mean_height = (!valid_heights.is_empty())
        .then(|| valid_heights.iter().sum::<f32>() / valid_heights.len() as f32);
    let elevation_range = minimum_height
        .zip(maximum_height)
        .map(|(min, max)| max - min);
    let mut steepest: Option<(f32, WorkbenchTerrainCoordinate)> = None;
    for z in 0..grid.height as usize {
        for x in 0..grid.width as usize {
            let index = z * grid.width as usize + x;
            let Some(height) = grid.heights[index] else {
                continue;
            };
            for (adjacent_x, adjacent_z) in [(x + 1, z), (x, z + 1)] {
                if adjacent_x >= grid.width as usize || adjacent_z >= grid.height as usize {
                    continue;
                }
                let adjacent_index = adjacent_z * grid.width as usize + adjacent_x;
                let Some(adjacent_height) = grid.heights[adjacent_index] else {
                    continue;
                };
                let slope_degrees = ((adjacent_height - height).abs()
                    / grid.effective_spacing_meters)
                    .atan()
                    .to_degrees();
                if steepest
                    .as_ref()
                    .is_none_or(|(current, _)| slope_degrees > *current)
                {
                    steepest = Some((
                        slope_degrees,
                        WorkbenchTerrainCoordinate {
                            x: grid.origin.x + adjacent_x as f32 * grid.effective_spacing_meters,
                            z: grid.origin.z + adjacent_z as f32 * grid.effective_spacing_meters,
                        },
                    ));
                }
            }
        }
    }
    WorkbenchTerrainSummary {
        valid_sample_count,
        minimum_height,
        maximum_height,
        mean_height,
        elevation_range,
        steepest_adjacent_slope_degrees: steepest.as_ref().map(|(slope, _)| *slope),
        steepest_adjacent_slope_position: steepest.map(|(_, position)| position),
    }
}

fn parse_terrain_water_grid(
    raw: &RawBridgeTerrainSample,
    sample_count: usize,
) -> Result<WorkbenchTerrainWaterGrid, ()> {
    let types = raw
        .water_types
        .split(';')
        .map(|value| match value {
            "~" => Ok(None),
            "n" => Ok(Some(WorkbenchTerrainWaterType::None)),
            "o" => Ok(Some(WorkbenchTerrainWaterType::Ocean)),
            "p" => Ok(Some(WorkbenchTerrainWaterType::Pond)),
            "r" => Ok(Some(WorkbenchTerrainWaterType::River)),
            _ => Err(()),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let values = |encoded: &str| {
        encoded
            .split(';')
            .map(|value| {
                if value == "~" {
                    Ok(None)
                } else {
                    value
                        .parse::<f32>()
                        .ok()
                        .filter(|value| value.is_finite())
                        .map(Some)
                        .ok_or(())
                }
            })
            .collect::<Result<Vec<_>, _>>()
    };
    let surface_heights = values(&raw.water_surface_heights)?;
    let depths_above_terrain = values(&raw.water_depths_above_terrain)?;
    if types.len() != sample_count
        || surface_heights.len() != sample_count
        || depths_above_terrain.len() != sample_count
    {
        return Err(());
    }
    Ok(WorkbenchTerrainWaterGrid {
        types,
        surface_heights,
        depths_above_terrain,
    })
}

fn summarize_terrain_water_grid(grid: &WorkbenchTerrainWaterGrid) -> WorkbenchTerrainWaterSummary {
    let mut summary = WorkbenchTerrainWaterSummary {
        wet_sample_count: 0,
        ocean_sample_count: 0,
        pond_sample_count: 0,
        river_sample_count: 0,
        maximum_depth_above_terrain: None,
    };
    for (kind, depth) in grid.types.iter().zip(&grid.depths_above_terrain) {
        match kind {
            Some(WorkbenchTerrainWaterType::Ocean) => summary.ocean_sample_count += 1,
            Some(WorkbenchTerrainWaterType::Pond) => summary.pond_sample_count += 1,
            Some(WorkbenchTerrainWaterType::River) => summary.river_sample_count += 1,
            _ => continue,
        }
        summary.wet_sample_count += 1;
        if let Some(depth) = depth {
            summary.maximum_depth_above_terrain = Some(
                summary
                    .maximum_depth_above_terrain
                    .map_or(*depth, |maximum| maximum.max(*depth)),
            );
        }
    }
    summary
}

fn play_session(
    reported: &Option<String>,
    world_editor_module_present: bool,
    world_editor_api_available: bool,
) -> Option<WorkbenchPlaySession> {
    match reported.as_deref() {
        Some("unavailable") => Some(WorkbenchPlaySession::Unavailable),
        Some("editing") | Some("unknown") => Some(WorkbenchPlaySession::Unknown),
        Some("likely-running") => Some(WorkbenchPlaySession::LikelyRunning),
        Some(_) => None,
        None if world_editor_api_available => Some(WorkbenchPlaySession::Unknown),
        None if world_editor_module_present => Some(WorkbenchPlaySession::LikelyRunning),
        None => Some(WorkbenchPlaySession::Unavailable),
    }
}

fn canonical_resource_name(value: &str) -> bool {
    let Some((guid, path)) = value
        .strip_prefix('{')
        .and_then(|value| value.split_once('}'))
    else {
        return false;
    };
    guid.len() == 16
        && guid.bytes().all(|byte| byte.is_ascii_hexdigit())
        && !path.is_empty()
        && !path.contains("..")
        && !path.contains('\\')
        && !path.contains(':')
}

fn canonical_component_class_name(value: &str) -> bool {
    let mut characters = value.chars();
    matches!(characters.next(), Some(character) if character.is_ascii_alphabetic() || character == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
        && value.len() <= 128
}

fn parse_prefab_component_id(value: &str) -> Option<(i32, String)> {
    let mut fields = value.splitn(3, ':');
    (fields.next()? == "cmp1").then_some(())?;
    let index = fields.next()?.parse::<i32>().ok()?;
    let class_name = fields.next()?;
    (index >= 0 && canonical_component_class_name(class_name))
        .then(|| (index, class_name.to_string()))
}

fn parse_world_selection_records(value: &str) -> Result<Vec<WorkbenchSelectedEntity>, ()> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    value
        .split(';')
        .map(|record| {
            let mut fields = record.split('|');
            let entity_id = fields.next().filter(|value| !value.is_empty()).ok_or(())?;
            let class_name = fields.next().filter(|value| !value.is_empty()).ok_or(())?;
            let sub_scene = fields.next().ok_or(())?.parse::<i32>().map_err(|_| ())?;
            let layer_id = fields.next().ok_or(())?.parse::<i32>().map_err(|_| ())?;
            let position = match fields.next() {
                None => None,
                Some(x) => {
                    let y = fields.next().ok_or(())?;
                    let z = fields.next().ok_or(())?;
                    let x = x.parse::<f32>().map_err(|_| ())?;
                    let y = y.parse::<f32>().map_err(|_| ())?;
                    let z = z.parse::<f32>().map_err(|_| ())?;
                    if !x.is_finite() || !y.is_finite() || !z.is_finite() {
                        return Err(());
                    }
                    Some(WorkbenchEntityPosition {
                        x: round_world_coordinate(x),
                        y: round_world_coordinate(y),
                        z: round_world_coordinate(z),
                    })
                }
            };
            let resource_name = fields
                .next()
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            let name = fields
                .next()
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            let sub_scene_name = fields
                .next()
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            let layer_name = fields
                .next()
                .filter(|value| !value.is_empty())
                .map(str::to_string);
            if fields.next().is_some()
                || entity_id.len() > 128
                || class_name.len() > 256
                || [
                    resource_name.as_deref(),
                    name.as_deref(),
                    sub_scene_name.as_deref(),
                    layer_name.as_deref(),
                ]
                .into_iter()
                .flatten()
                .any(|value| {
                    value.len() > 1024 || value.contains(|character: char| character.is_control())
                })
                || entity_id.contains(|character: char| character.is_control())
                || class_name.contains(|character: char| character.is_control())
            {
                return Err(());
            }
            Ok(WorkbenchSelectedEntity {
                entity_id: entity_id.to_string(),
                class_name: class_name.to_string(),
                sub_scene,
                layer_id,
                resource_name,
                name,
                sub_scene_name,
                layer_name,
                position,
            })
        })
        .collect()
}

fn parse_entity_search_records(value: &str) -> Result<Vec<WorkbenchEntitySearchHit>, ()> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    value
        .split(';')
        .map(|record| {
            let fields: Vec<_> = record.split('|').collect();
            if fields.len() != 18 || fields[..4].iter().any(|field| field.is_empty()) {
                return Err(());
            }
            let component_classes = if fields[6].is_empty() {
                Vec::new()
            } else {
                fields[6].split(',').map(str::to_owned).collect()
            };
            let matched_fields = if fields[7].is_empty() {
                Vec::new()
            } else {
                fields[7].split(',').map(str::to_owned).collect()
            };
            let matched_component_classes = if fields[8].is_empty() {
                Vec::new()
            } else {
                fields[8].split(',').map(str::to_owned).collect()
            };
            let relation_match = match (
                fields[11], fields[12], fields[13], fields[14], fields[15], fields[16], fields[17],
            ) {
                ("", "", "", "", "", "", "") => None,
                (direction, depth, entity_id, class_name, sub_scene, layer_id, components) => {
                    let direction = match direction {
                        "parent" => WorkbenchEntityRelationDirection::Parent,
                        "ancestor" => WorkbenchEntityRelationDirection::Ancestor,
                        "child" => WorkbenchEntityRelationDirection::Child,
                        "descendant" => WorkbenchEntityRelationDirection::Descendant,
                        _ => return Err(()),
                    };
                    let depth = depth.parse::<i32>().map_err(|_| ())?;
                    if !(1..=8).contains(&depth)
                        || entity_id.is_empty()
                        || entity_id.len() > 256
                        || entity_id
                            .bytes()
                            .any(|byte| matches!(byte, b'|' | b';' | b',' | b'\r' | b'\n'))
                        || !valid_component_class_name(class_name)
                    {
                        return Err(());
                    }
                    let matched_component_classes = if components.is_empty() {
                        Vec::new()
                    } else {
                        components.split(',').map(str::to_owned).collect()
                    };
                    if matched_component_classes.len() > 32
                        || matched_component_classes
                            .iter()
                            .any(|class_name| !valid_component_class_name(class_name))
                    {
                        return Err(());
                    }
                    Some(WorkbenchEntityRelationMatch {
                        direction,
                        depth,
                        entity_id: entity_id.to_owned(),
                        class_name: class_name.to_owned(),
                        sub_scene: sub_scene.parse().map_err(|_| ())?,
                        layer_id: layer_id.parse().map_err(|_| ())?,
                        matched_component_classes,
                    })
                }
            };
            Ok(WorkbenchEntitySearchHit {
                entity: WorkbenchSelectedEntity {
                    entity_id: fields[0].to_owned(),
                    class_name: fields[1].to_owned(),
                    sub_scene: fields[2].parse().map_err(|_| ())?,
                    layer_id: fields[3].parse().map_err(|_| ())?,
                    resource_name: (!fields[4].is_empty()).then(|| fields[4].to_owned()),
                    name: (!fields[5].is_empty()).then(|| fields[5].to_owned()),
                    sub_scene_name: None,
                    layer_name: None,
                    position: None,
                },
                component_classes,
                matched_component_classes,
                matched_fields,
                parent_class_name: (!fields[9].is_empty()).then(|| fields[9].to_owned()),
                child_count: fields[10].parse().map_err(|_| ())?,
                relation_match,
            })
        })
        .collect()
}

fn valid_component_class_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn round_world_coordinate(value: f32) -> f32 {
    (value * 1000.0).round() / 1000.0
}

fn is_false(value: &bool) -> bool {
    !value
}

fn is_available_status(value: &String) -> bool {
    value == "available"
}

fn parse_components(value: &str) -> Result<Vec<WorkbenchComponent>, ()> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    value
        .split(';')
        .map(|record| {
            let mut fields = record.split('|');
            let index = fields.next().ok_or(())?.parse::<u32>().map_err(|_| ())?;
            let class_name = fields
                .next()
                .filter(|value| !value.is_empty())
                .ok_or(())?
                .to_string();
            if fields.next().is_some() {
                return Err(());
            }
            Ok(WorkbenchComponent {
                component_id: format!("cmp1:{index}:{class_name}"),
                class_name,
                property_count: None,
                direct_override_count: None,
            })
        })
        .collect()
}

fn parse_prefab_components(
    components: &str,
    component_properties: &str,
) -> Result<Vec<WorkbenchPrefabComponent>, ()> {
    let mut properties_by_component =
        std::collections::BTreeMap::<u32, Vec<WorkbenchPrefabProperty>>::new();
    for record in component_properties
        .split(';')
        .filter(|record| !record.is_empty())
    {
        let mut fields = record.split('|');
        let index = fields.next().ok_or(())?.parse::<u32>().map_err(|_| ())?;
        let path = fields.next().filter(|value| !value.is_empty()).ok_or(())?;
        let data_type = fields.next().ok_or(())?;
        let value = fields.next().ok_or(())?;
        let directly_overridden = fields.next().ok_or(())? == "1";
        let value_origin = fields
            .next()
            .map(parse_prefab_property_origin)
            .transpose()?;
        if fields.next().is_some() {
            return Err(());
        }
        properties_by_component
            .entry(index)
            .or_default()
            .push(WorkbenchPrefabProperty {
                path: path.to_string(),
                data_type: data_type.to_string(),
                value: normalized_property_value(data_type, value),
                directly_overridden,
                value_origin,
                write_descriptor: None,
            });
    }

    if components.is_empty() {
        return Ok(Vec::new());
    }
    components
        .split(';')
        .map(|record| {
            let mut fields = record.split('|');
            let index = fields.next().ok_or(())?.parse::<u32>().map_err(|_| ())?;
            let class_name = fields
                .next()
                .filter(|value| !value.is_empty())
                .ok_or(())?
                .to_string();
            if fields.next().is_some() {
                return Err(());
            }
            Ok(WorkbenchPrefabComponent {
                component_id: format!("cmp1:{index}:{class_name}"),
                class_name,
                properties: properties_by_component.remove(&index).unwrap_or_default(),
            })
        })
        .collect()
}

fn parse_prefab_property_origin(value: &str) -> Result<WorkbenchPrefabPropertyOrigin, ()> {
    match value {
        "direct" => Ok(WorkbenchPrefabPropertyOrigin::Direct),
        "inherited" => Ok(WorkbenchPrefabPropertyOrigin::Inherited),
        "default" => Ok(WorkbenchPrefabPropertyOrigin::Default),
        _ => Err(()),
    }
}

fn parse_component_summaries(
    components: &str,
    component_properties: &str,
) -> Result<Vec<WorkbenchComponent>, ()> {
    parse_prefab_components(components, component_properties).map(|components| {
        components
            .into_iter()
            .map(|component| WorkbenchComponent {
                component_id: component.component_id,
                class_name: component.class_name,
                property_count: Some(component.properties.len() as u32),
                direct_override_count: Some(
                    component
                        .properties
                        .iter()
                        .filter(|property| property.directly_overridden)
                        .count() as u32,
                ),
            })
            .collect()
    })
}

fn is_native_component_descriptor(value: &str) -> bool {
    let mut fields = value.splitn(3, ':');
    matches!(fields.next(), Some("cmp1"))
        && fields
            .next()
            .is_some_and(|index| index.parse::<u32>().is_ok())
        && fields
            .next()
            .is_some_and(|class_name| !class_name.is_empty())
}

fn parse_properties(value: &str) -> Result<Vec<WorkbenchDirectProperty>, ()> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    value
        .split(';')
        .map(|record| {
            let mut fields = record.split('|');
            let name = fields
                .next()
                .filter(|value| !value.is_empty())
                .ok_or(())?
                .to_string();
            let data_type = fields.next().ok_or(())?.to_string();
            let raw_value = fields.next().ok_or(())?.to_string();
            let directly_overridden = fields
                .next()
                .map(|value| match value {
                    "0" => Ok(false),
                    "1" => Ok(true),
                    _ => Err(()),
                })
                .transpose()?;
            let value_origin = fields
                .next()
                .map(parse_prefab_property_origin)
                .transpose()?;
            if fields.next().is_some() {
                return Err(());
            }
            Ok(WorkbenchDirectProperty {
                name,
                value: normalized_property_value(&data_type, &raw_value),
                data_type,
                directly_overridden,
                value_origin,
                write_descriptor: None,
            })
        })
        .collect()
}

fn split_bounded_records(value: &str) -> Vec<String> {
    value
        .split(';')
        .filter(|record| !record.is_empty())
        .map(str::to_string)
        .collect()
}

fn parse_prefab_members(
    value: &str,
    parent_member_id: &str,
) -> Result<Vec<WorkbenchPrefabMember>, ()> {
    value
        .split(';')
        .filter(|record| !record.is_empty())
        .map(|record| {
            let mut fields = record.split('|');
            let index = fields.next().ok_or(())?.parse::<u32>().map_err(|_| ())?;
            let class_name = fields.next().filter(|value| !value.is_empty()).ok_or(())?;
            let name = fields.next().ok_or(())?;
            if fields.next().is_some()
                || class_name.len() > 256
                || name.len() > 1024
                || class_name.contains(|character: char| character.is_control())
                || name.contains(|character: char| character.is_control())
            {
                return Err(());
            }
            let member_id = format!("member:{index}");
            Ok(WorkbenchPrefabMember {
                member_id: if parent_member_id.is_empty() {
                    member_id
                } else {
                    format!("{parent_member_id}/{member_id}")
                },
                class_name: class_name.to_string(),
                name: (!name.is_empty()).then_some(name.to_string()),
            })
        })
        .collect()
}

fn parse_prefab_properties(value: &str) -> Result<Vec<WorkbenchPrefabProperty>, ()> {
    let mut properties = value
        .split(';')
        .filter(|record| !record.is_empty())
        .map(|record| {
            let mut fields = record.split('|');
            let path = fields.next().ok_or(())?;
            let data_type = fields.next().ok_or(())?;
            let value = fields.next().ok_or(())?;
            let directly_overridden = fields.next().ok_or(())? == "1";
            let value_origin = fields
                .next()
                .map(parse_prefab_property_origin)
                .transpose()?;
            if fields.next().is_some() {
                return Err(());
            }
            Ok(WorkbenchPrefabProperty {
                path: path.to_string(),
                data_type: data_type.to_string(),
                value: normalized_property_value(data_type, value),
                directly_overridden,
                value_origin,
                write_descriptor: None,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    properties.retain(|property| !is_engine_owned_prefab_property(&property.path));
    Ok(properties)
}

fn is_engine_owned_prefab_property(path: &str) -> bool {
    matches!(path, "userScript" | "constructor" | "destructor")
}

#[derive(Clone, Copy)]
enum PropertyValueKind {
    Bool,
    Integer,
    Float,
    Vector,
    String,
}

fn property_value_kind(data_type: &str) -> Option<PropertyValueKind> {
    match data_type.trim().to_ascii_lowercase().as_str() {
        "bool" => Some(PropertyValueKind::Bool),
        "int" | "integer" | "enum" => Some(PropertyValueKind::Integer),
        "float" => Some(PropertyValueKind::Float),
        "vector" => Some(PropertyValueKind::Vector),
        "string" | "resource" | "entity" => Some(PropertyValueKind::String),
        _ => None,
    }
}

fn supported_property_type(data_type: &str) -> bool {
    property_value_kind(data_type).is_some()
}

fn normalized_property_value(data_type: &str, raw: &str) -> Value {
    if matches!(
        property_value_kind(data_type),
        Some(PropertyValueKind::Bool)
    ) {
        return Value::Bool(matches!(raw, "1" | "true" | "True"));
    }
    if matches!(
        property_value_kind(data_type),
        Some(PropertyValueKind::Integer)
    ) {
        if let Ok(value) = raw.parse::<i64>() {
            return Value::Number(value.into());
        }
    }
    if matches!(
        property_value_kind(data_type),
        Some(PropertyValueKind::Float)
    ) {
        if let Ok(value) = raw.parse::<f64>() {
            if let Some(value) = serde_json::Number::from_f64(value) {
                return Value::Number(value);
            }
        }
    }
    if matches!(
        property_value_kind(data_type),
        Some(PropertyValueKind::Vector)
    ) {
        let values = raw
            .split_whitespace()
            .filter_map(|part| part.parse::<f64>().ok())
            .collect::<Vec<_>>();
        if values.len() == 3 && values.iter().all(|value| value.is_finite()) {
            return json!({"x": values[0], "y": values[1], "z": values[2]});
        }
    }
    Value::String(raw.to_string())
}

fn property_value_wire_format(data_type: &str, value: &Value) -> Option<String> {
    if matches!(
        property_value_kind(data_type),
        Some(PropertyValueKind::Bool)
    ) {
        return value.as_bool().map(|value| {
            if value {
                "1".to_string()
            } else {
                "0".to_string()
            }
        });
    }
    if matches!(
        property_value_kind(data_type),
        Some(PropertyValueKind::Integer)
    ) {
        return value.as_i64().map(|value| value.to_string());
    }
    if matches!(
        property_value_kind(data_type),
        Some(PropertyValueKind::Float)
    ) {
        return value
            .as_f64()
            .filter(|value| value.is_finite())
            .map(|value| value.to_string());
    }
    if matches!(
        property_value_kind(data_type),
        Some(PropertyValueKind::Vector)
    ) {
        let object = value.as_object()?;
        let component = |name| {
            object
                .get(name)
                .and_then(Value::as_f64)
                .filter(|value| value.is_finite())
        };
        let x = component("x")?;
        let y = component("y")?;
        let z = component("z")?;
        return Some(format!("{x} {y} {z}"));
    }
    if matches!(
        property_value_kind(data_type),
        Some(PropertyValueKind::String)
    ) {
        return value
            .as_str()
            .filter(|value| value.len() <= 1024 && !value.contains(['|', ';', '\0']))
            .map(str::to_string);
    }
    None
}

fn invalid_property_descriptor_result() -> WorkbenchEntityMutationResult {
    WorkbenchEntityMutationResult {
        bridge_version: WORKBENCH_BRIDGE_VERSION.to_string(),
        protocol_version: WORKBENCH_BRIDGE_PROTOCOL_VERSION,
        status: "invalid-property-descriptor".to_string(),
        active_layer_id: None,
        entity: None,
        confirmation_token: None,
        destination: None,
        destination_exists: None,
        resource_name: None,
        persistence_path: None,
        template_saved: None,
        inspection: None,
    }
}

fn invalid_confirmation_result() -> WorkbenchEntityMutationResult {
    WorkbenchEntityMutationResult {
        bridge_version: WORKBENCH_BRIDGE_VERSION.to_string(),
        protocol_version: WORKBENCH_BRIDGE_PROTOCOL_VERSION,
        status: "invalid-confirmation".to_string(),
        active_layer_id: None,
        entity: None,
        confirmation_token: None,
        destination: None,
        destination_exists: None,
        resource_name: None,
        persistence_path: None,
        template_saved: None,
        inspection: None,
    }
}

fn invalid_prefab_resource_mutation_result(
    resource_name: &str,
) -> WorkbenchPrefabResourceMutationResult {
    WorkbenchPrefabResourceMutationResult {
        bridge_version: WORKBENCH_BRIDGE_VERSION.to_string(),
        protocol_version: WORKBENCH_BRIDGE_PROTOCOL_VERSION,
        status: "invalid-confirmation".to_string(),
        resource_name: resource_name.to_string(),
        persistence_path: "workbench-resource".to_string(),
        component_id: None,
        component_class: None,
        template_saved: false,
        inspection: None,
        component_inspection: None,
        confirmation_token: None,
    }
}

fn invalid_component_property_descriptor_result() -> WorkbenchComponentResult {
    WorkbenchComponentResult {
        bridge_version: WORKBENCH_BRIDGE_VERSION.to_string(),
        protocol_version: WORKBENCH_BRIDGE_PROTOCOL_VERSION,
        status: "invalid-property-descriptor".to_string(),
        entity: None,
        components: Vec::new(),
        properties: Vec::new(),
        confirmation_token: None,
    }
}

fn invalid_component_descriptor_result() -> WorkbenchComponentResult {
    WorkbenchComponentResult {
        bridge_version: WORKBENCH_BRIDGE_VERSION.to_string(),
        protocol_version: WORKBENCH_BRIDGE_PROTOCOL_VERSION,
        status: "invalid-component-descriptor".to_string(),
        entity: None,
        components: Vec::new(),
        properties: Vec::new(),
        confirmation_token: None,
    }
}

fn parse_optional_world_selection_record(
    value: &str,
) -> Result<Option<WorkbenchSelectedEntity>, ()> {
    if value.is_empty() {
        return Ok(None);
    }
    let mut records = parse_world_selection_records(value)?;
    if records.len() != 1 {
        return Err(());
    }
    Ok(records.pop())
}

fn parse_vector_record(value: &str) -> Result<WorkbenchEntityPosition, ()> {
    let values = value
        .split_whitespace()
        .map(|part| part.parse::<f32>().map_err(|_| ()))
        .collect::<Result<Vec<_>, _>>()?;
    if values.len() != 3 || values.iter().any(|value| !value.is_finite()) {
        return Err(());
    }
    Ok(WorkbenchEntityPosition {
        x: values[0],
        y: values[1],
        z: values[2],
    })
}

fn parse_shape_points(value: &str) -> Option<Vec<WorkbenchEntityPosition>> {
    if value.is_empty() {
        return Some(Vec::new());
    }
    value
        .split(';')
        .map(|record| {
            let fields = record.split('|').collect::<Vec<_>>();
            if fields.len() != 3 {
                return None;
            }
            let x = fields[0].parse::<f32>().ok()?;
            let y = fields[1].parse::<f32>().ok()?;
            let z = fields[2].parse::<f32>().ok()?;
            if !x.is_finite() || !y.is_finite() || !z.is_finite() {
                return None;
            }
            Some(WorkbenchEntityPosition { x, y, z })
        })
        .collect()
}

fn parse_spline_vector(values: &[&str], offset: usize) -> Result<WorkbenchEntityPosition, ()> {
    let x = values[offset].parse::<f32>().map_err(|_| ())?;
    let y = values[offset + 1].parse::<f32>().map_err(|_| ())?;
    let z = values[offset + 2].parse::<f32>().map_err(|_| ())?;
    if !x.is_finite() || !y.is_finite() || !z.is_finite() {
        return Err(());
    }
    Ok(WorkbenchEntityPosition { x, y, z })
}

fn parse_spline_anchors(value: &str) -> Result<Vec<WorkbenchSplineAnchor>, ()> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    value
        .split(';')
        .map(|record| {
            let fields = record.split(',').collect::<Vec<_>>();
            if fields.len() != 11 {
                return Err(());
            }
            let tangent_mode = match fields[1] {
                "auto" => WorkbenchSplineTangentMode::Auto,
                "explicit" => WorkbenchSplineTangentMode::Explicit,
                _ => return Err(()),
            };
            Ok(WorkbenchSplineAnchor {
                index: fields[0].parse::<usize>().map_err(|_| ())?,
                position: parse_spline_vector(&fields, 2)?,
                tangent_mode,
                in_tangent: parse_spline_vector(&fields, 5)?,
                out_tangent: parse_spline_vector(&fields, 8)?,
            })
        })
        .collect()
}

fn parse_spline_samples(value: &str) -> Result<Vec<WorkbenchEntityPosition>, ()> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    value
        .split(';')
        .map(|record| {
            let fields = record.split(',').collect::<Vec<_>>();
            if fields.len() != 3 {
                return Err(());
            }
            parse_spline_vector(&fields, 0)
        })
        .collect()
}

fn encode_shape_points(points: &[WorkbenchEntityPosition]) -> String {
    points
        .iter()
        .map(|point| format!("{},{},{}", point.x, point.y, point.z))
        .collect::<Vec<_>>()
        .join(";")
}

fn shape_point_space_name(space: WorkbenchShapePointSpace) -> &'static str {
    match space {
        WorkbenchShapePointSpace::Local => "local",
        WorkbenchShapePointSpace::World => "world",
    }
}

fn shape_transform_operation_name(operation: WorkbenchShapeTransformOperation) -> &'static str {
    match operation {
        WorkbenchShapeTransformOperation::Translate => "translate",
        WorkbenchShapeTransformOperation::RotateXz => "rotateXZ",
        WorkbenchShapeTransformOperation::Scale => "scale",
        WorkbenchShapeTransformOperation::Mirror => "mirror",
        WorkbenchShapeTransformOperation::Reverse => "reverse",
    }
}

struct ResolvedWorkbenchPaths {
    workbench_root: PathBuf,
    profile: PathBuf,
    profile_source: &'static str,
    bridge_directory: PathBuf,
    legacy_bridge_directory: PathBuf,
    game: Option<PathBuf>,
    game_source: String,
    tools: Option<PathBuf>,
    tools_source: String,
    executable: Option<PathBuf>,
    executable_source: String,
}

/// The Workbench profile directory this host keeps, when the extension has not
/// been pointed at a different one. Workbench owns this location; every reader
/// of the profile resolves it here.
pub fn default_profile_directory() -> Option<PathBuf> {
    workbench_host()
        .user_directory()
        .map(|user| profile_directory_in(&user))
}

fn profile_directory_in(user: &Path) -> PathBuf {
    user.join("Documents")
        .join("My Games")
        .join("ArmaReforgerWorkbench")
        .join("profile")
}

fn path_status(path: Option<PathBuf>, source: &str) -> WorkbenchPathStatus {
    let exists = path.as_ref().is_some_and(|value| value.exists());
    WorkbenchPathStatus {
        path,
        exists,
        source: source.to_string(),
    }
}

fn is_workbench_executable(path: &std::path::Path) -> bool {
    path.is_file()
        && path.file_name().is_some_and(|name| {
            name.to_string_lossy()
                .eq_ignore_ascii_case(WORKBENCH_EXECUTABLE_NAME)
        })
}

fn paths_equal(left: &std::path::Path, right: &std::path::Path) -> bool {
    let left = fs::canonicalize(left).unwrap_or_else(|_| left.to_path_buf());
    let right = fs::canonicalize(right).unwrap_or_else(|_| right.to_path_buf());
    left == right
}

fn manifest_matches_payload(manifest: &BridgeManifest) -> bool {
    manifest_matches_payload_for(manifest, bridge_payload())
}

fn manifest_matches_payload_for(manifest: &BridgeManifest, payload: &[(&str, &str)]) -> bool {
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

fn sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn split_bounded_list(value: &str, max_items: usize, max_item_bytes: usize) -> (Vec<String>, bool) {
    let all = value
        .split(';')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    let truncated = all.len() > max_items
        || all
            .iter()
            .take(max_items)
            .any(|value| value.len() > max_item_bytes);
    let items = all
        .into_iter()
        .take(max_items)
        .map(|value| {
            if value.len() <= max_item_bytes {
                value.to_string()
            } else {
                let mut end = max_item_bytes;
                while !value.is_char_boundary(end) {
                    end -= 1;
                }
                value[..end].to_string()
            }
        })
        .collect();
    (items, truncated)
}

fn version_order(left: &str, right: &str) -> std::cmp::Ordering {
    match (Version::parse(left), Version::parse(right)) {
        (Ok(left), Ok(right)) => left.cmp(&right),
        _ if left == right => std::cmp::Ordering::Equal,
        // An unrecognized installed version is never safe to overwrite
        // automatically because its precedence cannot be proven.
        _ => std::cmp::Ordering::Greater,
    }
}

fn parse_validation_cursor(cursor: &str) -> Option<(String, usize)> {
    let mut parts = cursor.split(':');
    (parts.next()? == "wv1").then_some(())?;
    let token = parts.next()?.to_string();
    let offset = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((token, offset))
}

fn parse_resource_list_cursor(cursor: &str) -> Option<(String, usize)> {
    let mut parts = cursor.split(':');
    (parts.next()? == "wrl1").then_some(())?;
    let signature = parts.next()?.to_string();
    let offset = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((signature, offset))
}

fn valid_resource_root_path(value: &str) -> bool {
    if value.is_empty() {
        return true;
    }
    let Some((addon, logical_path)) = value
        .strip_prefix('$')
        .and_then(|value| value.split_once(':'))
    else {
        return false;
    };
    !addon.is_empty()
        && !addon.contains([':', ';', '|', '/', '\\'])
        && !logical_path.starts_with('/')
        && !logical_path.contains("..")
        && !logical_path.contains([';', '|', '\\'])
}

fn valid_loaded_addon(addon: &WorkbenchLoadedAddon) -> bool {
    addon.guid.len() == 16
        && addon.guid.bytes().all(|byte| byte.is_ascii_hexdigit())
        && !addon.id.trim().is_empty()
        && !addon.title.trim().is_empty()
        && (addon.source_root.as_os_str().is_empty() || addon.source_root.is_absolute())
}

/// Resolves the one physical instance Workbench has selected for each loaded
/// GUID. Mounted add-ons arrive with their root from Workbench itself. Packed
/// add-ons are resolved only from the active Workbench Tools project registry;
/// ambiguous or absent instances remain unavailable rather than guessed.
fn resolve_loaded_addon_roots(
    host: &WorkbenchHost,
    addons: &mut [WorkbenchLoadedAddon],
    current_project: Option<&Path>,
    profile: &Path,
) -> Result<(), ()> {
    let mut candidates = HashMap::<String, HashSet<PathBuf>>::new();
    for project in registered_project_files(host, profile).map_err(|_| ())? {
        register_project_candidate(&mut candidates, project)?;
    }
    if let Some(project) = current_project {
        register_project_candidate(&mut candidates, project.to_path_buf())?;
        if let Some(addons_directory) = project.parent().and_then(Path::parent) {
            for entry in fs::read_dir(addons_directory).map_err(|_| ())?.flatten() {
                let directory = entry.path();
                if directory.is_dir() {
                    for project in fs::read_dir(directory).map_err(|_| ())?.flatten() {
                        let project = project.path();
                        if project
                            .extension()
                            .is_some_and(|extension| extension.eq_ignore_ascii_case("gproj"))
                        {
                            register_project_candidate(&mut candidates, project)?;
                        }
                    }
                }
            }
        }
    }
    for addon in addons {
        if addon.source_root.is_absolute() {
            continue;
        }
        let roots = candidates.get(&addon.guid.to_ascii_uppercase()).ok_or(())?;
        if roots.len() != 1 {
            return Err(());
        }
        addon.source_root = roots.iter().next().cloned().ok_or(())?;
    }
    Ok(())
}

pub(crate) fn registered_project_files(
    host: &WorkbenchHost,
    profile: &Path,
) -> Result<Vec<PathBuf>, String> {
    let mut projects = Vec::new();
    for entry in fs::read_dir(profile)
        .map_err(|error| error.to_string())?
        .flatten()
    {
        let path = entry.path();
        let is_registry = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(".projectList_app1874910_"));
        if !is_registry {
            continue;
        }
        let source = fs::read_to_string(path).map_err(|error| error.to_string())?;
        projects.extend(
            source
                .lines()
                .filter_map(|line| project_list_file_path(host, line)),
        );
    }
    Ok(projects)
}

#[derive(Deserialize)]
struct RawBridgeEntityTransform {
    #[serde(rename = "bridgeVersion")]
    bridge_version: String,
    #[serde(rename = "protocolVersion")]
    protocol_version: u32,
    status: String,
    entity: String,
    position: Option<String>,
    angles: Option<String>,
    scale: f32,
}

/// Returns the installed game's top-level add-on projects through the same
/// unambiguous Steam installation discovery used by Workbench path handling.
/// An unavailable or ambiguous installation is intentionally represented as
/// an empty result; offline user add-ons can still be resolved from the
/// project-list registry.
pub(crate) fn installed_game_addon_project_files() -> Result<Vec<PathBuf>, String> {
    #[cfg(test)]
    {
        Ok(Vec::new())
    }
    #[cfg(not(test))]
    {
        let Some(game) = (match discover_steam_app(REFORGER_GAME_APP_ID) {
            SteamAppDiscovery::Found(path) => Some(path),
            SteamAppDiscovery::RegistrationUnavailable
            | SteamAppDiscovery::ManifestUnavailable
            | SteamAppDiscovery::InvalidInstallation
            | SteamAppDiscovery::AmbiguousInstallations => None,
        }) else {
            return Ok(Vec::new());
        };
        let addons = game.join("addons");
        let mut projects = BTreeSet::new();
        for entry in fs::read_dir(&addons)
            .map_err(|error| error.to_string())?
            .flatten()
        {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            for project in fs::read_dir(&path)
                .map_err(|error| error.to_string())?
                .flatten()
            {
                let project = project.path();
                if project.is_file()
                    && project
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("gproj"))
                {
                    projects.insert(fs::canonicalize(project).map_err(|error| error.to_string())?);
                }
            }
        }
        Ok(projects.into_iter().collect())
    }
}

#[derive(Deserialize)]
struct RawBridgeSpline {
    #[serde(rename = "bridgeVersion")]
    bridge_version: String,
    #[serde(rename = "protocolVersion")]
    protocol_version: u32,
    status: String,
    #[serde(default)]
    entity: String,
    #[serde(rename = "shapeClass", default)]
    shape_class: String,
    #[serde(default, deserialize_with = "deserialize_boolish")]
    closed: bool,
    #[serde(rename = "anchorCount", default)]
    anchor_count: usize,
    #[serde(default)]
    anchors: String,
    #[serde(default)]
    samples: String,
    #[serde(rename = "sampleSpace", default)]
    sample_space: String,
    #[serde(rename = "sampleCount", default)]
    sample_count: usize,
    #[serde(rename = "pathLength", default)]
    path_length: f32,
}

impl RawBridgeSpline {
    fn into_result(self) -> Result<WorkbenchSpline, WorkbenchFailure> {
        let entity = parse_optional_world_selection_record(&self.entity)
            .map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        let anchors = parse_spline_anchors(&self.anchors)
            .map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        let samples = parse_spline_samples(&self.samples)
            .map_err(|_| failure(WorkbenchFailureCode::Protocol))?;
        if !self.path_length.is_finite() {
            return Err(failure(WorkbenchFailureCode::Protocol));
        }
        Ok(WorkbenchSpline {
            bridge_version: self.bridge_version,
            protocol_version: self.protocol_version,
            status: self.status,
            entity,
            shape_class: (!self.shape_class.is_empty()).then_some(self.shape_class),
            closed: self.closed,
            anchor_count: self.anchor_count,
            anchors,
            samples,
            sample_space: self.sample_space,
            sample_count: self.sample_count,
            path_length: self.path_length,
        })
    }
}

/// Reads one project path from the Workbench project registry. Workbench
/// records the path in its own space, so the host path is resolved here.
fn project_list_file_path(host: &WorkbenchHost, line: &str) -> Option<PathBuf> {
    let value = line.trim().strip_prefix("FilePath ")?.trim();
    let value = value.strip_prefix('"')?.strip_suffix('"')?;
    let path = host.to_host_path(value)?;
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("gproj"))
        .then_some(path)
}

fn register_project_candidate(
    candidates: &mut HashMap<String, HashSet<PathBuf>>,
    project: PathBuf,
) -> Result<(), ()> {
    let source = fs::read_to_string(&project).map_err(|_| ())?;
    let guid = source
        .lines()
        .find_map(|line| {
            let value = line.trim().strip_prefix("GUID ")?.trim();
            value.strip_prefix('"')?.strip_suffix('"')
        })
        .ok_or(())?;
    if guid.len() != 16 || !guid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(());
    }
    let root = project.parent().ok_or(())?.to_path_buf();
    candidates
        .entry(guid.to_ascii_uppercase())
        .or_default()
        .insert(root);
    Ok(())
}

fn parse_resource_search_hit(value: &str) -> Result<WorkbenchResourceSearchHit, WorkbenchFailure> {
    let mut fields = value.split('|');
    let resource_name = fields.next().unwrap_or_default();
    let addon_guid = fields.next().unwrap_or_default();
    let addon_id = fields.next().unwrap_or_default();
    let logical_path = fields.next().unwrap_or_default();
    let extension = fields.next().unwrap_or_default();
    if fields.next().is_some()
        || resource_name.is_empty()
        || addon_guid.is_empty()
        || logical_path.is_empty()
        || extension.is_empty()
        || !resource_name.starts_with('{')
        || !resource_name.contains('}')
        || logical_path.contains("..")
        || logical_path.contains([';', '|', '\\'])
        || logical_path.starts_with('/')
    {
        return Err(failure(WorkbenchFailureCode::Protocol));
    }
    let (stem, observed_extension) = logical_path
        .rsplit_once('.')
        .filter(|(_, observed_extension)| !observed_extension.is_empty())
        .ok_or_else(|| failure(WorkbenchFailureCode::Protocol))?;
    if observed_extension != extension {
        return Err(failure(WorkbenchFailureCode::Protocol));
    }
    let name = stem
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .ok_or_else(|| failure(WorkbenchFailureCode::Protocol))?;
    Ok(WorkbenchResourceSearchHit {
        resource_name: resource_name.to_owned(),
        addon_guid: addon_guid.to_owned(),
        addon_id: (!addon_id.is_empty()).then(|| addon_id.to_owned()),
        logical_path: logical_path.to_owned(),
        name: name.to_owned(),
        extension: extension.to_owned(),
    })
}

fn parse_entity_list_cursor(cursor: &str) -> Option<(String, usize)> {
    let mut fields = cursor.split(':');
    (fields.next()? == "wel1")
        .then_some(())
        .and_then(|_| Some((fields.next()?.to_string(), fields.next()?.parse().ok()?)))
        .filter(|_| fields.next().is_none())
}

fn latest_workbench_log(workbench_root: &std::path::Path) -> Option<PathBuf> {
    let logs = workbench_root.join("logs");
    let mut directories = fs::read_dir(logs)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .collect::<Vec<_>>();
    directories.sort_by_key(|entry| entry.file_name());
    directories
        .into_iter()
        .rev()
        .map(|entry| entry.path().join("console.log"))
        .find(|path| path.is_file())
}

fn workbench_log_markers(source: &str, lines: &[String]) -> Vec<WorkbenchLogMarker> {
    if source != "workbench" {
        return Vec::new();
    }
    const MARKERS: [(&str, &str); 5] = [
        ("reload-started", WORKBENCH_RELOAD_START_MARKER),
        ("script-validation", "Script validation"),
        ("gamelib-compilation", "Compiling GameLib scripts"),
        ("game-compilation", "Compiling Game scripts"),
        (
            "workbench-game-module-loaded",
            "Module: WorkbenchGame; loaded",
        ),
    ];
    lines
        .iter()
        .enumerate()
        .flat_map(|(line_index, line)| {
            MARKERS
                .iter()
                .filter(move |(_, marker)| line.contains(marker))
                .map(move |(kind, _)| WorkbenchLogMarker {
                    kind: (*kind).to_string(),
                    line_index,
                })
        })
        .collect()
}

const WORKBENCH_RELOAD_START_MARKER: &str = "Reloading game scripts";
const MAX_LOG_READ_BYTES: u64 = 512 * 1024;

fn bounded_log_tail(
    path: &std::path::Path,
    line_count: usize,
) -> std::io::Result<(Vec<String>, bool)> {
    let (all, truncated) = bounded_log_window(path)?;
    let line_truncated = all.len() > line_count;
    Ok((
        limit_log_lines(all, Some(line_count), false),
        truncated || line_truncated,
    ))
}

fn read_all_log_lines(
    path: &std::path::Path,
    line_count: Option<usize>,
) -> std::io::Result<(Vec<String>, bool)> {
    let all = log_lines_from_offset(path, 0)?;
    let line_truncated = line_count.is_some_and(|limit| limit < all.len());
    Ok((limit_log_lines(all, line_count, false), line_truncated))
}

fn bounded_log_since_reload_start(
    path: &std::path::Path,
    line_count: Option<usize>,
) -> std::io::Result<(Vec<String>, bool)> {
    let Some(barrier_offset) = latest_reload_start_offset(path)? else {
        return Ok((Vec::new(), false));
    };
    let length = fs::metadata(path)?.len();
    let section_length = length.saturating_sub(barrier_offset);
    let (selected, storage_truncated) = if section_length > MAX_LOG_READ_BYTES {
        let barrier = log_lines_from_offset(path, barrier_offset)?
            .into_iter()
            .next()
            .unwrap_or_default();
        let (tail, _) = bounded_log_window(path)?;
        let mut selected = Vec::with_capacity(tail.len() + 1);
        selected.push(barrier);
        selected.extend(tail);
        (selected, true)
    } else {
        (log_lines_from_offset(path, barrier_offset)?, false)
    };
    let line_truncated = line_count.is_some_and(|limit| limit < selected.len());
    Ok((
        limit_log_lines(selected, line_count, false),
        storage_truncated || line_truncated,
    ))
}

fn latest_reload_start_offset(path: &std::path::Path) -> std::io::Result<Option<u64>> {
    let mut reader = BufReader::new(fs::File::open(path)?);
    let mut offset = 0_u64;
    let mut latest = None;
    let mut line = String::new();
    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line)?;
        if bytes_read == 0 {
            break;
        }
        if line.contains(WORKBENCH_RELOAD_START_MARKER) {
            latest = Some(offset);
        }
        offset += bytes_read as u64;
    }
    Ok(latest)
}

fn log_lines_from_offset(path: &std::path::Path, offset: u64) -> std::io::Result<Vec<String>> {
    let mut file = fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    BufReader::new(file).lines().collect()
}

fn limit_log_lines(
    lines: Vec<String>,
    line_count: Option<usize>,
    preserve_first: bool,
) -> Vec<String> {
    let Some(line_count) = line_count else {
        return lines;
    };
    if lines.len() <= line_count {
        return lines;
    }
    if preserve_first {
        let mut selected = Vec::with_capacity(line_count);
        selected.push(lines[0].clone());
        let tail = lines
            .into_iter()
            .rev()
            .take(line_count.saturating_sub(1))
            .collect::<Vec<_>>();
        selected.extend(tail.into_iter().rev());
        selected
    } else {
        lines
            .into_iter()
            .rev()
            .take(line_count)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }
}

fn bounded_log_window(path: &std::path::Path) -> std::io::Result<(Vec<String>, bool)> {
    let mut file = fs::File::open(path)?;
    let length = file.metadata()?.len();
    let offset = length.saturating_sub(MAX_LOG_READ_BYTES);
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::with_capacity((length - offset) as usize);
    file.read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let mut all = text.lines();
    if offset > 0 {
        all.next();
    }
    Ok((all.map(str::to_string).collect(), offset > 0))
}

/// The prefix Workbench puts before the open project in its window title.
const WORKBENCH_WINDOW_TITLE_PREFIX: &str = "Enfusion Workbench - ";

/// The open project named by the one visible Workbench window title.
///
/// A host with no supported window route, or a process showing anything other
/// than exactly one Workbench window, leaves the project unresolved here.
fn workbench_project_title(process: ProcessIdentity) -> Option<String> {
    let mut titles = process::window_titles(process)?
        .into_iter()
        .filter_map(|title| {
            title
                .strip_prefix(WORKBENCH_WINDOW_TITLE_PREFIX)
                .map(str::to_string)
        })
        .collect::<Vec<_>>();
    (titles.len() == 1).then(|| titles.remove(0))
}

/// Returns the exact `.gproj` passed to the already-running Workbench process.
///
/// The command line is more authoritative than the window title: user addons
/// commonly live outside the Tools installation's `addons` directory and
/// therefore cannot be rediscovered by title alone.
fn workbench_project_gproj(host: &WorkbenchHost, process: ProcessIdentity) -> Option<PathBuf> {
    let arguments = process::command_line(process)?;
    let value = arguments
        .iter()
        .position(|argument| argument.eq_ignore_ascii_case("-gproj"))
        .and_then(|index| arguments.get(index + 1))?;
    let path = host.to_host_path(value)?;
    (path
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("gproj"))
        && path.is_file())
    .then_some(path)
}

fn resolve_project_gproj(workbench_root: &std::path::Path, title: &str) -> Option<PathBuf> {
    let mut directories = vec![workbench_root.join("addons")];
    let mut matches = Vec::new();
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(directory).ok()?.flatten() {
            let path = entry.path();
            let kind = entry.file_type().ok()?;
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                directories.push(path);
            } else if kind.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("gproj"))
                && fs::read_to_string(&path)
                    .ok()
                    .is_some_and(|text| project_title(&text) == Some(title))
            {
                matches.push(path);
            }
        }
        if matches.len() > 1 || directories.len() > 1_024 {
            return None;
        }
    }
    (matches.len() == 1).then(|| matches.remove(0))
}

fn project_title(content: &str) -> Option<&str> {
    content.lines().find_map(|line| {
        let line = line.trim();
        let value = line.strip_prefix("TITLE ")?.trim();
        value.strip_prefix('"')?.strip_suffix('"')
    })
}

fn base_game_addons_directory(game_directory: Option<&std::path::Path>) -> Option<PathBuf> {
    let addons = game_directory?.join("addons");
    addons
        .join("data")
        .join("ArmaReforger.gproj")
        .is_file()
        .then_some(addons)
}

/// Builds the arguments Workbench is started with, addressed in Workbench's
/// own path space. A host path that Workbench cannot address leaves the launch
/// unavailable rather than passing a path Workbench would reject.
fn workbench_launch_arguments(
    host: &WorkbenchHost,
    project: Option<&std::path::Path>,
    game_directory: Option<&std::path::Path>,
    profile_root: Option<&std::path::Path>,
) -> Option<Vec<std::ffi::OsString>> {
    let mut arguments = vec![
        std::ffi::OsString::from("-noThrow"),
        std::ffi::OsString::from("-forceUpdate"),
    ];
    if let Some(profile_root) = profile_root {
        arguments.extend([
            std::ffi::OsString::from("-profile"),
            host.to_workbench_path(profile_root)?,
        ]);
    }
    let game_addons = base_game_addons_directory(game_directory)?;
    if let Some(project) = project {
        arguments.extend([
            std::ffi::OsString::from("-gproj"),
            host.to_workbench_path(project)?,
        ]);
    }
    arguments.extend([
        std::ffi::OsString::from("-addons"),
        std::ffi::OsString::from(WORKBENCH_REQUIRED_ADDONS),
        std::ffi::OsString::from("-addonsDir"),
        host.to_workbench_path(&game_addons)?,
    ]);
    Some(arguments)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SteamAppDiscovery {
    Found(PathBuf),
    RegistrationUnavailable,
    ManifestUnavailable,
    InvalidInstallation,
    AmbiguousInstallations,
}

impl SteamAppDiscovery {
    fn into_path_and_source(self) -> (Option<PathBuf>, String) {
        match self {
            Self::Found(path) => (Some(path), "steam-registry".to_string()),
            Self::RegistrationUnavailable => (None, "steam-registration-unavailable".to_string()),
            Self::ManifestUnavailable => (None, "steam-manifest-unavailable".to_string()),
            Self::InvalidInstallation => (None, "steam-installation-invalid".to_string()),
            Self::AmbiguousInstallations => (None, "steam-installation-ambiguous".to_string()),
        }
    }
}

fn discover_steam_app(app_id: &str) -> SteamAppDiscovery {
    discover_steam_app_from_roots(&host_platform::steam_roots(), app_id)
}

#[cfg(test)]
fn discover_steam_app_from_root(steam_root: &std::path::Path, app_id: &str) -> SteamAppDiscovery {
    discover_steam_app_from_roots(&[steam_root.to_path_buf()], app_id)
}

fn discover_steam_app_from_roots(steam_roots: &[PathBuf], app_id: &str) -> SteamAppDiscovery {
    if steam_roots.is_empty() {
        return SteamAppDiscovery::RegistrationUnavailable;
    }
    let mut libraries = steam_roots
        .iter()
        .flat_map(|steam_root| host_platform::steam_libraries(steam_root))
        .collect::<Vec<_>>();
    libraries.sort_by_key(|path| path.to_string_lossy().to_ascii_lowercase());
    libraries.dedup_by(|left, right| paths_equal(left, right));

    let mut manifest_found = false;
    let mut invalid_installation = false;
    let mut candidates = Vec::new();
    for library in libraries {
        let steamapps = library.join("steamapps");
        let manifest = steamapps.join(format!("appmanifest_{app_id}.acf"));
        let Ok(content) = fs::read_to_string(manifest) else {
            continue;
        };
        manifest_found = true;
        let Some(install_dir) = host_platform::acf_string(&content, "installdir") else {
            invalid_installation = true;
            continue;
        };
        let candidate = steamapps.join("common").join(install_dir);
        if valid_steam_app_install(app_id, &candidate) {
            candidates.push(candidate);
        } else {
            invalid_installation = true;
        }
    }
    candidates.sort_by_key(|path| path.to_string_lossy().to_ascii_lowercase());
    candidates.dedup_by(|left, right| paths_equal(left, right));
    match candidates.len() {
        1 => SteamAppDiscovery::Found(candidates.remove(0)),
        2.. => SteamAppDiscovery::AmbiguousInstallations,
        _ if invalid_installation => SteamAppDiscovery::InvalidInstallation,
        _ if manifest_found => SteamAppDiscovery::InvalidInstallation,
        _ => SteamAppDiscovery::ManifestUnavailable,
    }
}

fn valid_steam_app_install(app_id: &str, candidate: &std::path::Path) -> bool {
    match app_id {
        REFORGER_GAME_APP_ID => candidate
            .join("addons")
            .join("data")
            .join("ArmaReforger.gproj")
            .is_file(),
        REFORGER_TOOLS_APP_ID => {
            is_workbench_executable(&candidate.join("Workbench").join(WORKBENCH_EXECUTABLE_NAME))
        }
        _ => false,
    }
}

/// The `HKEY_CURRENT_USER` keys the `enfusion` URL protocol is registered under.
const ENFUSION_PROTOCOL_KEY: &str = r"Software\Classes\enfusion";
const ENFUSION_PROTOCOL_COMMAND_KEY: &str = r"Software\Classes\enfusion\shell\open\command";
const ENFUSION_PROTOCOL_DESCRIPTION: &str = "URL:enfusion Protocol";
/// The `HKEY_CURRENT_USER` value Workbench reads its NET API switch from.
const WORKBENCH_OPTIONS_KEY: &str =
    r"Software\Bohemia Interactive\Arma Reforger Workbench\Workbench";

fn enfusion_protocol_command(
    host: &WorkbenchHost,
    executable: &Path,
    addons: &Path,
    project: &Path,
) -> Option<String> {
    Some(format!(
        "\"{}\" -addonsDir \"{}\" -gproj \"{}\" -uri=\"%1\"",
        workbench_command_path(host, executable)?,
        workbench_command_argument_path(host, addons)?,
        workbench_command_argument_path(host, project)?,
    ))
}

fn workbench_command_argument_path(host: &WorkbenchHost, path: &Path) -> Option<String> {
    Some(workbench_command_path(host, path)?.replace('\\', "/"))
}

/// The path Workbench addresses a host path by, in the plain form a command
/// line carries rather than the extended form canonicalization produces.
fn workbench_command_path(host: &WorkbenchHost, path: &Path) -> Option<String> {
    let value = host.to_workbench_path(path)?;
    let value = value.to_string_lossy();
    Some(match value.strip_prefix(r"\\?\UNC\") {
        Some(share) => format!(r"\\{share}"),
        None => value.strip_prefix(r"\\?\").unwrap_or(&value).to_string(),
    })
}

/// Registers the `enfusion` URL protocol wherever this host resolves it: in
/// Workbench's own registry, and — where Workbench runs inside a prefix — with
/// the host desktop that opens links from outside it.
fn register_enfusion_protocol(
    host: &WorkbenchHost,
    paths: &ResolvedWorkbenchPaths,
) -> std::io::Result<bool> {
    let command = resolved_enfusion_protocol_command(host, paths)?;
    let mut changed = false;
    changed |= set_workbench_registry_string(
        host,
        ENFUSION_PROTOCOL_KEY,
        None,
        ENFUSION_PROTOCOL_DESCRIPTION,
    )?;
    changed |=
        set_workbench_registry_string(host, ENFUSION_PROTOCOL_KEY, Some("URL Protocol"), "")?;
    changed |= set_workbench_registry_string(host, ENFUSION_PROTOCOL_COMMAND_KEY, None, &command)?;
    changed |= register_host_enfusion_handler(host, paths)?;
    Ok(changed)
}

fn enfusion_protocol_registered(host: &WorkbenchHost, paths: &ResolvedWorkbenchPaths) -> bool {
    let Ok(expected_command) = resolved_enfusion_protocol_command(host, paths) else {
        return false;
    };
    workbench_registry_nonblank_string(host, ENFUSION_PROTOCOL_KEY, None).as_deref()
        == Some(ENFUSION_PROTOCOL_DESCRIPTION)
        && workbench_registry_string(host, ENFUSION_PROTOCOL_KEY, Some("URL Protocol")).as_deref()
            == Some("")
        && workbench_registry_nonblank_string(host, ENFUSION_PROTOCOL_COMMAND_KEY, None).as_deref()
            == Some(expected_command.as_str())
        && host_enfusion_handler_registered(host, paths)
}

fn resolved_enfusion_protocol_command(
    host: &WorkbenchHost,
    paths: &ResolvedWorkbenchPaths,
) -> std::io::Result<String> {
    let (executable, addons, project) = resolved_enfusion_protocol_targets(paths)?;
    enfusion_protocol_command(host, executable, &addons, &project).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "the resolved Workbench paths have no address in Workbench's path space",
        )
    })
}

fn resolved_enfusion_protocol_targets(
    paths: &ResolvedWorkbenchPaths,
) -> std::io::Result<(&Path, PathBuf, PathBuf)> {
    let executable = paths
        .executable
        .as_deref()
        .filter(|path| is_workbench_executable(path))
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "the resolved Workbench executable is unavailable",
            )
        })?;
    let addons = paths
        .game
        .as_deref()
        .map(|game| game.join("addons"))
        .filter(|path| path.is_dir())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "the resolved Arma Reforger add-ons directory is unavailable",
            )
        })?;
    let project = addons.join("data").join("ArmaReforger.gproj");
    if !project.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "the resolved base-game Workbench project is unavailable",
        ));
    }
    Ok((executable, addons, project))
}

/// Registers the host desktop handler that opens an `enfusion` link which was
/// followed outside the prefix. A native host already resolves the scheme
/// through the registry written above.
fn register_host_enfusion_handler(
    host: &WorkbenchHost,
    paths: &ResolvedWorkbenchPaths,
) -> std::io::Result<bool> {
    let Some(handler) = host_enfusion_handler(host, paths) else {
        return Ok(false);
    };
    host_platform::url_scheme::register(&handler)
}

/// Whether the host desktop resolves the scheme. A host with no handler to
/// register — a native host, or one with no route to start Workbench — has
/// nothing outstanding and is reported as satisfied rather than as stale work
/// that bootstrap would repeat without effect.
fn host_enfusion_handler_registered(host: &WorkbenchHost, paths: &ResolvedWorkbenchPaths) -> bool {
    host_enfusion_handler(host, paths)
        .is_none_or(|handler| host_platform::url_scheme::registered(&handler))
}

/// The desktop handler this host needs, if any.
fn host_enfusion_handler(
    host: &WorkbenchHost,
    paths: &ResolvedWorkbenchPaths,
) -> Option<host_platform::url_scheme::SchemeHandler> {
    host.wine_prefix()?;
    let (executable, addons, project) = resolved_enfusion_protocol_targets(paths).ok()?;
    let arguments = [
        "-addonsDir".to_string(),
        workbench_command_argument_path(host, &addons)?,
        "-gproj".to_string(),
        workbench_command_argument_path(host, &project)?,
        "-uri=%u".to_string(),
    ];
    host.url_scheme_handler(
        "enfusion",
        "Arma Reforger Workbench",
        executable,
        &arguments,
    )
}

/// Reads one string value from the registry Workbench reads its options from.
fn workbench_registry_string(
    host: &WorkbenchHost,
    key: &str,
    value_name: Option<&str>,
) -> Option<String> {
    match host {
        WorkbenchHost::Native => native_registry_string(key, value_name),
        WorkbenchHost::Wine(prefix) => {
            host_platform::wine_registry::read_string(&prefix.user_registry_path(), key, value_name)
        }
        WorkbenchHost::Unavailable => None,
    }
}

/// The same value, with a blank string treated as absent.
fn workbench_registry_nonblank_string(
    host: &WorkbenchHost,
    key: &str,
    value_name: Option<&str>,
) -> Option<String> {
    workbench_registry_string(host, key, value_name).filter(|value| !value.trim().is_empty())
}

/// Writes one string value into the registry Workbench reads its options from,
/// reporting whether the registry changed.
fn set_workbench_registry_string(
    host: &WorkbenchHost,
    key: &str,
    value_name: Option<&str>,
    value: &str,
) -> std::io::Result<bool> {
    match host {
        WorkbenchHost::Native => native_registry_write(key, value_name, value),
        WorkbenchHost::Wine(prefix) => {
            // Wine loads the hive when its server starts and rewrites it from
            // memory on shutdown, so an edit made now would be discarded while
            // the prefix is running.
            if host_platform::wine_registry::prefix_in_use(prefix.root()) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ResourceBusy,
                    "close Workbench so the Wine prefix registry can be written",
                ));
            }
            host_platform::wine_registry::write_string(
                &prefix.user_registry_path(),
                key,
                value_name,
                value,
            )
        }
        WorkbenchHost::Unavailable => Err(host_platform::unsupported("the Workbench registry")),
    }
}

#[cfg(windows)]
fn native_registry_string(key: &str, value_name: Option<&str>) -> Option<String> {
    host_platform::windows_registry::current_user_string_including_empty(
        key,
        value_name.unwrap_or_default(),
    )
}

#[cfg(not(windows))]
fn native_registry_string(_key: &str, _value_name: Option<&str>) -> Option<String> {
    None
}

#[cfg(windows)]
fn native_registry_write(
    key: &str,
    value_name: Option<&str>,
    value: &str,
) -> std::io::Result<bool> {
    host_platform::windows_registry::set_current_user_string(key, value_name, value)
}

#[cfg(not(windows))]
fn native_registry_write(
    _key: &str,
    _value_name: Option<&str>,
    _value: &str,
) -> std::io::Result<bool> {
    Err(host_platform::unsupported("the Windows registry"))
}

fn enable_workbench_net_api(host: &WorkbenchHost) -> std::io::Result<bool> {
    set_workbench_registry_string(host, WORKBENCH_OPTIONS_KEY, Some("NetAPI_Enabled"), "1")
}

fn bridge_payload() -> &'static [(&'static str, &'static str)] {
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

#[cfg(test)]
mod tests {
    use super::{
        enfusion_protocol_command, WorkbenchDiagnosticLocation, WorkbenchDiagnosticSeverity,
        WorkbenchFailureCode, WorkbenchGateway, WorkbenchGatewayOptions, WorkbenchStatus,
    };
    use serde_json::{json, Value};
    use std::fs;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;
    use std::time::{Duration, Instant};

    /// The extended-length form only exists in the native Windows path space.
    #[cfg(windows)]
    #[test]
    fn enfusion_protocol_command_quotes_every_path_and_the_uri_argument() {
        let executable = std::path::Path::new(
            r"\\?\C:\Program Files (x86)\Steam\steamapps\common\Arma Reforger Tools\Workbench\ArmaReforgerWorkbenchSteamDiag.exe",
        );
        let addons = std::path::Path::new(
            r"\\?\C:\Program Files (x86)\Steam\steamapps\common\Arma Reforger\addons",
        );
        let project = addons.join("data").join("ArmaReforger.gproj");

        assert_eq!(
            enfusion_protocol_command(&super::WorkbenchHost::Native, executable, addons, &project),
            r#""C:\Program Files (x86)\Steam\steamapps\common\Arma Reforger Tools\Workbench\ArmaReforgerWorkbenchSteamDiag.exe" -addonsDir "C:/Program Files (x86)/Steam/steamapps/common/Arma Reforger/addons" -gproj "C:/Program Files (x86)/Steam/steamapps/common/Arma Reforger/addons/data/ArmaReforger.gproj" -uri="%1""#,
        );
    }

    #[test]
    fn enfusion_protocol_command_addresses_a_wine_prefix_in_workbench_paths() {
        let host = wine_test_host();
        let tools = std::path::Path::new("/library/steamapps/common/Arma Reforger Tools");
        let executable = tools
            .join("Workbench")
            .join("ArmaReforgerWorkbenchSteamDiag.exe");
        let addons = std::path::Path::new("/library/steamapps/common/Arma Reforger/addons");
        let project = addons.join("data").join("ArmaReforger.gproj");

        assert_eq!(
            enfusion_protocol_command(&host, &executable, addons, &project).as_deref(),
            Some(concat!(
                r#""Z:\library\steamapps\common\Arma Reforger Tools\Workbench\ArmaReforgerWorkbenchSteamDiag.exe""#,
                r#" -addonsDir "Z:/library/steamapps/common/Arma Reforger/addons""#,
                r#" -gproj "Z:/library/steamapps/common/Arma Reforger/addons/data/ArmaReforger.gproj""#,
                r#" -uri="%1""#,
            )),
        );
    }

    #[test]
    fn workbench_bool_accepts_json_and_enfusion_representations() {
        assert!(super::workbench_bool(&json!(true)));
        assert!(super::workbench_bool(&json!(1)));
        assert!(!super::workbench_bool(&json!(false)));
        assert!(!super::workbench_bool(&json!(0)));
        assert!(!super::workbench_bool(&Value::Null));
    }

    #[test]
    fn loaded_addon_graph_preserves_workbench_order_and_exact_source_roots() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(request, json!({"APIFunc": "RST_WorkbenchLoadedAddonGraph"}));
            json!({
                "bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,
                "protocolVersion": 1,
                "graphJson": "[{\"guid\":\"58D0FB3206B6F859\",\"id\":\"ArmaReforger\",\"title\":\"Arma Reforger\",\"sourceRoot\":\"C:/Game/addons/data\"},{\"guid\":\"684CE8AA3B1D6573\",\"id\":\"GCSuppression\",\"title\":\"GC Suppression\",\"sourceRoot\":\"C:/Workbench/addons/GC-Suppression\"}]"
            })
        });
        let profile = test_root("loaded-addon-graph");
        fs::create_dir_all(&profile).unwrap();
        let controller = super::WorkbenchController::with_host(
            super::WorkbenchControllerOptions {
                gateway: super::WorkbenchGatewayOptions {
                    port,
                    status_deadline: Duration::from_secs(1),
                    ..super::WorkbenchGatewayOptions::default()
                },
                profile_directory: Some(profile.clone()),
                ..super::WorkbenchControllerOptions::default()
            },
            wine_test_host(),
        );

        let graph = controller.loaded_addon_graph().unwrap();

        assert_eq!(graph.addons.len(), 2);
        assert_eq!(graph.addons[0].guid, "58D0FB3206B6F859");
        assert_eq!(
            graph.addons[0].source_root,
            std::path::PathBuf::from("/prefix/drive_c/Game/addons/data"),
            "Workbench reports its own path space and the host reads the mapped path",
        );
        assert_eq!(graph.addons[1].id, "GCSuppression");
        assert_eq!(
            graph.addons[1].source_root,
            std::path::PathBuf::from("/prefix/drive_c/Workbench/addons/GC-Suppression")
        );
        peer.join().unwrap();
        fs::remove_dir_all(profile).unwrap();
    }

    #[test]
    fn resolves_packed_roots_from_the_active_workbench_project_registry() {
        let root = std::env::temp_dir().join(format!(
            "rst-workbench-project-registry-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let profile = root.join("profile");
        let addons = root.join("addons");
        let packed = root.join("downloaded/packed");
        let data = addons.join("data");
        let core = addons.join("core");
        let mounted = addons.join("workspace-mounted");
        fs::create_dir_all(&profile).unwrap();
        for directory in [&packed, &data, &core, &mounted] {
            fs::create_dir_all(directory).unwrap();
        }
        fs::write(
            packed.join("Packed.gproj"),
            "GameProject {\n GUID \"1111111111111111\"\n}",
        )
        .unwrap();
        fs::write(
            data.join("ArmaReforger.gproj"),
            "GameProject {\n GUID \"2222222222222222\"\n}",
        )
        .unwrap();
        fs::write(
            core.join("core.gproj"),
            "GameProject {\n GUID \"3333333333333333\"\n}",
        )
        .unwrap();
        fs::write(
            profile.join(".projectList_app1874910_user.conf"),
            format!("FilePath \"{}\"", packed.join("Packed.gproj").display()),
        )
        .unwrap();
        let mut addons = vec![
            super::WorkbenchLoadedAddon {
                guid: "1111111111111111".to_string(),
                id: "Packed".to_string(),
                title: "Packed".to_string(),
                source_root: std::path::PathBuf::new(),
            },
            super::WorkbenchLoadedAddon {
                guid: "2222222222222222".to_string(),
                id: "ArmaReforger".to_string(),
                title: "Arma Reforger".to_string(),
                source_root: std::path::PathBuf::new(),
            },
            super::WorkbenchLoadedAddon {
                guid: "3333333333333333".to_string(),
                id: "core".to_string(),
                title: "Core".to_string(),
                source_root: std::path::PathBuf::new(),
            },
            super::WorkbenchLoadedAddon {
                guid: "4444444444444444".to_string(),
                id: "Mounted".to_string(),
                title: "Mounted".to_string(),
                source_root: mounted.clone(),
            },
        ];

        super::resolve_loaded_addon_roots(
            &super::WorkbenchHost::Native,
            &mut addons,
            Some(&data.join("ArmaReforger.gproj")),
            &profile,
        )
        .unwrap();

        assert_eq!(addons[0].source_root, packed);
        assert_eq!(addons[1].source_root, data);
        assert_eq!(addons[2].source_root, core);
        assert_eq!(addons[3].source_root, mounted);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn world_selection_records_are_bounded_and_require_stable_identity_fields() {
        let records = super::parse_world_selection_records(
            "0x0000000000000001|TestEntity|0|12|12.5|3|-4.25|PrefabName|Authored name|Main scene|Gameplay;0x0000000000000002|LightEntity|2|4",
        )
        .unwrap();

        assert_eq!(records.len(), 2);
        assert_eq!(records[0].entity_id, "0x0000000000000001");
        assert_eq!(records[0].class_name, "TestEntity");
        assert_eq!(records[0].sub_scene, 0);
        assert_eq!(records[0].layer_id, 12);
        assert_eq!(
            records[0].position,
            Some(super::WorkbenchEntityPosition {
                x: 12.5,
                y: 3.0,
                z: -4.25,
            })
        );
        assert_eq!(records[0].resource_name.as_deref(), Some("PrefabName"));
        assert_eq!(records[0].name.as_deref(), Some("Authored name"));
        assert_eq!(records[0].sub_scene_name.as_deref(), Some("Main scene"));
        assert_eq!(records[0].layer_name.as_deref(), Some("Gameplay"));
        assert!(records[1].position.is_none());
        assert!(super::parse_world_selection_records("missing-fields").is_err());
        assert!(super::parse_world_selection_records("id|class|nope|0").is_err());
        assert!(super::parse_world_selection_records("id|class|0|0|NaN|0|0").is_err());
    }

    #[test]
    fn selected_entity_hierarchy_records_require_one_stable_target() {
        let entity =
            super::parse_optional_world_selection_record("0x0000000000000001|TestEntity|0|12")
                .unwrap()
                .expect("entity");
        assert_eq!(entity.entity_id, "0x0000000000000001");
        assert!(super::parse_optional_world_selection_record("")
            .unwrap()
            .is_none());
        assert!(super::parse_optional_world_selection_record(
            "0x0000000000000001|One|0|1;0x0000000000000002|Two|0|2"
        )
        .is_err());
    }

    #[test]
    fn versioned_bridge_handlers_report_the_manifest_version() {
        let expected = format!(
            "response.bridgeVersion = \"{}\"",
            super::WORKBENCH_BRIDGE_VERSION
        );
        for (name, source) in super::bridge_payload() {
            if source.contains("bridgeVersion") {
                assert!(
                    source.contains(&expected),
                    "{name} must report the manifest version"
                );
            }
        }
    }

    #[test]
    fn workbench_log_markers_classify_only_observed_reload_milestones() {
        let lines = vec![
            "SCRIPT: Reloading game scripts".to_string(),
            "SCRIPT: Script validation".to_string(),
            "SCRIPT: Compiling GameLib scripts".to_string(),
            "SCRIPT: Compiling Game scripts".to_string(),
            "SCRIPT: Module: WorkbenchGame; loaded".to_string(),
        ];
        let markers = super::workbench_log_markers("workbench", &lines);
        assert_eq!(
            markers
                .iter()
                .map(|marker| marker.kind.as_str())
                .collect::<Vec<_>>(),
            vec![
                "reload-started",
                "script-validation",
                "gamelib-compilation",
                "game-compilation",
                "workbench-game-module-loaded",
            ]
        );
        assert!(super::workbench_log_markers("integration", &lines).is_empty());
    }

    #[test]
    fn latest_workbench_logs_start_at_the_latest_reload_start() {
        let root = test_root("latest-log-start");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("console.log");
        fs::write(
            &path,
            "old startup\nSCRIPT: Reloading game scripts\nnew warning\nnew result\n",
        )
        .unwrap();

        let (lines, truncated) = super::bounded_log_since_reload_start(&path, None).unwrap();

        assert!(!truncated);
        assert_eq!(
            lines,
            vec![
                "SCRIPT: Reloading game scripts",
                "new warning",
                "new result"
            ]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn latest_workbench_log_line_limit_returns_the_latest_lines() {
        let root = test_root("latest-log-limit");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("console.log");
        fs::write(
            &path,
            "SCRIPT: Reloading game scripts\nline one\nline two\nline three\n",
        )
        .unwrap();

        let (lines, truncated) = super::bounded_log_since_reload_start(&path, Some(2)).unwrap();

        assert!(truncated);
        assert_eq!(lines, vec!["line two", "line three"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn latest_workbench_logs_find_a_reload_start_outside_the_bounded_tail() {
        let root = test_root("latest-log-large-section");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("console.log");
        let contents = format!(
            "old startup\nSCRIPT: Reloading game scripts\n{}current result\n",
            "noise\n".repeat(100_000)
        );
        fs::write(&path, contents).unwrap();

        let (lines, truncated) = super::bounded_log_since_reload_start(&path, None).unwrap();

        assert!(truncated);
        assert_eq!(
            lines.first().map(String::as_str),
            Some("SCRIPT: Reloading game scripts")
        );
        assert_eq!(lines.last().map(String::as_str), Some("current result"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reload_action_dispatch_is_only_a_precondition_for_typed_verification() {
        let path = super::reload_action_path(Ok(super::RawBridgeReloadAction {
            _bridge_version: "1.51.0".to_string(),
            protocol_version: 1,
            _accepted: json!(false),
            action_path: "Plugins/Settings/Reload WB Scripts".to_string(),
        }))
        .unwrap();

        assert_eq!(path, "Plugins/Settings/Reload WB Scripts");
    }

    #[test]
    fn reload_action_timeout_can_proceed_to_typed_verification() {
        assert_eq!(
            super::reload_action_path(Err(super::failure(super::WorkbenchFailureCode::Timeout)))
                .unwrap(),
            "Plugins/Settings/Reload WB Scripts"
        );
    }

    #[test]
    fn activation_upgrades_a_protocol_compatible_prior_bridge_generation() {
        let (port, peer) = start_peer_sequence(vec![
            (
                json!({"APIFunc": "RST_WorkbenchCapabilities"}),
                json!({
                    "bridgeVersion": "1.52.1",
                    "protocolVersion": 1,
                    "runtimeGeneration": 7,
                    "capabilities": "state"
                }),
            ),
            (
                json!({"APIFunc": "RST_WorkbenchState", "executeSaveAllAction": true}),
                json!({
                    "bridgeVersion": "1.52.1",
                    "protocolVersion": 1,
                    "saveAllActionAccepted": true,
                    "saveAllActionPath": "File/Save All",
                    "worldSaveActionAccepted": false,
                    "worldSaveStatus": "skipped-no-open-world"
                }),
            ),
            (
                json!({"APIFunc": "RST_WorkbenchState", "executeReloadAction": true}),
                json!({
                    "bridgeVersion": "1.52.1",
                    "protocolVersion": 1,
                    "reloadActionAccepted": false,
                    "reloadActionPath": "Plugins/Settings/Reload WB Scripts"
                }),
            ),
            (
                json!({"APIFunc": "RST_WorkbenchCapabilities"}),
                json!({
                    "bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,
                    "protocolVersion": 1,
                    "runtimeGeneration": 8,
                    "capabilities": "state"
                }),
            ),
        ]);
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            ..super::WorkbenchControllerOptions::default()
        });

        let result = controller.activate_scripts().unwrap();

        assert!(result.reload_dispatched);
        assert_eq!(result.runtime_generation, 8);
        peer.join().unwrap();
    }

    #[test]
    fn activation_rejects_a_handler_without_a_typed_runtime_generation() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(request, json!({"APIFunc": "RST_WorkbenchCapabilities"}));
            json!({
                "bridgeVersion": "1.51.0",
                "protocolVersion": 1,
                "runtimeGeneration": 0,
                "capabilities": "state"
            })
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            ..super::WorkbenchControllerOptions::default()
        });

        assert_eq!(
            controller.activate_scripts().unwrap_err().code,
            WorkbenchFailureCode::Protocol
        );
        peer.join().unwrap();
    }

    #[test]
    fn capability_handshake_requires_the_runtime_generation_field() {
        assert!(
            serde_json::from_value::<super::RawBridgeCapabilities>(json!({
                "bridgeVersion": "1.51.0",
                "protocolVersion": 1,
                "capabilities": "state"
            }))
            .is_err()
        );
    }

    #[test]
    fn activation_ignores_a_changed_generation_from_an_incompatible_bridge() {
        let (port, peer) = start_peer_sequence(vec![
            (
                json!({"APIFunc": "RST_WorkbenchCapabilities"}),
                json!({
                    "bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,
                    "protocolVersion": 1,
                    "runtimeGeneration": 7,
                    "capabilities": "state"
                }),
            ),
            (
                json!({"APIFunc": "RST_WorkbenchState", "executeSaveAllAction": true}),
                json!({
                    "bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,
                    "protocolVersion": 1,
                    "saveAllActionAccepted": true,
                    "saveAllActionPath": "File/Save All",
                    "worldSaveActionAccepted": false,
                    "worldSaveStatus": "skipped-no-open-world"
                }),
            ),
            (
                json!({"APIFunc": "RST_WorkbenchState", "executeReloadAction": true}),
                json!({
                    "bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,
                    "protocolVersion": 1,
                    "reloadActionAccepted": true,
                    "reloadActionPath": "Plugins/Settings/Reload WB Scripts"
                }),
            ),
            (
                json!({"APIFunc": "RST_WorkbenchCapabilities"}),
                json!({
                    "bridgeVersion": "1.50.0",
                    "protocolVersion": 1,
                    "runtimeGeneration": 8,
                    "capabilities": "state"
                }),
            ),
            (
                json!({"APIFunc": "RST_WorkbenchCapabilities"}),
                json!({
                    "bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,
                    "protocolVersion": 1,
                    "runtimeGeneration": 9,
                    "capabilities": "state"
                }),
            ),
        ]);
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            ..super::WorkbenchControllerOptions::default()
        });

        assert_eq!(controller.activate_scripts().unwrap().runtime_generation, 9);
        peer.join().unwrap();
    }

    #[test]
    fn world_selection_summary_returns_bounded_stable_editor_facts() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(request, json!({"APIFunc": "RST_WorkbenchWorldSelection"}));
            json!({
                "bridgeVersion": "1.5.0",
                "protocolVersion": 1,
                "editorAvailable": true,
                "status": "available",
                "selectedCount": 2,
                "selectedEntities": "0x0000000000000001|TestEntity|0|12;0x0000000000000002|LightEntity|2|4",
                "selectedEntitiesTruncated": false
            })
        });
        let root = test_root("world-selection-summary");
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            user_directory: Some(root.clone()),
            ..super::WorkbenchControllerOptions::default()
        });

        let result = controller.world_selection_summary().unwrap();

        assert!(result.editor_available);
        assert_eq!(result.selected_count, 2);
        assert_eq!(result.selected_entities.len(), 2);
        assert!(!result.selected_entities_truncated);
        peer.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resource_search_binds_opaque_cursors_to_the_same_filter() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(request["APIFunc"], "RST_WorkbenchListResources");
            assert_eq!(request["extensions"], "ent");
            assert_eq!(request["query"], "test");
            assert_eq!(request["rootPath"], "");
            assert_eq!(request["addonGuid"], "");
            assert_eq!(request["offset"], 0);
            assert_eq!(request["limit"], 2);
            json!({
                "bridgeVersion": "1.6.0",
                "protocolVersion": 1,
                "loadedAddons": "ArmaReforger;TestBullshit",
                "resources": "{DD49A6CE18710A05}worlds/test/empty_test.ent",
                "resourceDetails": "{DD49A6CE18710A05}worlds/test/empty_test.ent|DD49A6CE18710A05|TestBullshit|worlds/test/empty_test.ent|ent",
                "hasMore": true
            })
        });
        let root = test_root("resource-listing");
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            user_directory: Some(root.clone()),
            ..super::WorkbenchControllerOptions::default()
        });

        let page = controller
            .search_resources(&["ent"], Some("test"), None, None, None, 2)
            .unwrap();

        assert_eq!(page.results.len(), 1);
        assert_eq!(page.limit, 2);
        assert!(page.truncated);
        assert!(page.next_cursor.is_some());
        assert!(super::parse_resource_list_cursor(page.next_cursor.as_deref().unwrap()).is_some());
        assert_ne!(page.project_revision, "ArmaReforger;TestBullshit");
        peer.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resource_search_projects_only_canonical_resource_facts() {
        let hit = super::parse_resource_search_hit(
            "{00B6CAF6E4A5BAB4}Prefabs/Props/Test.et|00B6CAF6E4A5BAB4|TestBullshit|Prefabs/Props/Test.et|et",
        )
        .unwrap();
        assert_eq!(hit.addon_guid, "00B6CAF6E4A5BAB4");
        assert_eq!(hit.addon_id.as_deref(), Some("TestBullshit"));
        assert_eq!(hit.logical_path, "Prefabs/Props/Test.et");
        assert_eq!(hit.name, "Test");
        assert_eq!(hit.extension, "et");
        assert!(super::parse_resource_search_hit("C:/absolute/Test.et").is_err());
        assert!(super::parse_resource_search_hit("{GUID}../Test.et|GUID||../Test.et|et").is_err());
        assert!(super::valid_resource_root_path(
            "$TestBullshit:Prefabs/Props"
        ));
        assert!(super::valid_resource_root_path("$TestBullshit:"));
        assert!(!super::valid_resource_root_path("C:/absolute"));
        assert!(!super::valid_resource_root_path("$TestBullshit:../Prefabs"));
    }

    #[test]
    fn resource_search_binds_root_and_projects_bridge_facts() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(request["APIFunc"], "RST_WorkbenchListResources");
            assert_eq!(request["extensions"], "et");
            assert_eq!(request["query"], "test");
            assert_eq!(request["rootPath"], "$TestBullshit:Prefabs");
            assert_eq!(request["addonGuid"], "00B6CAF6E4A5BAB4");
            assert_eq!(request["offset"], 0);
            assert_eq!(request["limit"], 3);
            json!({
                "bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,
                "protocolVersion": 1,
                "loadedAddons": "ArmaReforger;TestBullshit",
                "resources": "{00B6CAF6E4A5BAB4}Prefabs/Props/Test.et",
                "resourceDetails": "{00B6CAF6E4A5BAB4}Prefabs/Props/Test.et|00B6CAF6E4A5BAB4|TestBullshit|Prefabs/Props/Test.et|et",
                "hasMore": false
            })
        });
        let root = test_root("resource-search");
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            user_directory: Some(root.clone()),
            ..super::WorkbenchControllerOptions::default()
        });

        let page = controller
            .search_resources(
                &["et"],
                Some("test"),
                Some("$TestBullshit:Prefabs"),
                Some("00B6CAF6E4A5BAB4"),
                None,
                3,
            )
            .unwrap();

        assert_eq!(page.results.len(), 1);
        assert_eq!(page.results[0].name, "Test");
        assert_eq!(page.results[0].addon_id.as_deref(), Some("TestBullshit"));
        assert!(!page.truncated);
        peer.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resource_inspection_accepts_workbenchs_numeric_boolean_fields() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(
                request,
                json!({
                    "APIFunc": "RST_WorkbenchInspectResource",
                    "resourceName": "{00B6CAF6E4A5BAB4}Prefabs/Props/Test.et"
                })
            );
            json!({
                "bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,
                "protocolVersion": 1,
                "found": 1,
                "status": "found",
                "resourceName": "{00B6CAF6E4A5BAB4}Prefabs/Props/Test.et",
                "className": "GenericEntity",
                "sourceAddons": "TestBullshit",
                "sourceAddonsTruncated": 0
            })
        });
        let root = test_root("resource-inspection-numeric-booleans");
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            user_directory: Some(root.clone()),
            ..super::WorkbenchControllerOptions::default()
        });

        let inspection = controller
            .inspect_resource("{00B6CAF6E4A5BAB4}Prefabs/Props/Test.et")
            .unwrap();

        assert!(inspection.found);
        assert_eq!(inspection.status, "found");
        assert_eq!(inspection.class_name.as_deref(), Some("GenericEntity"));
        assert_eq!(inspection.source_addons, vec!["TestBullshit"]);
        assert!(!inspection.source_addons_truncated);
        peer.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn entity_listing_accepts_workbenchs_numeric_has_more_flag() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(request["APIFunc"], "RST_WorkbenchListEntities");
            assert_eq!(request["limit"], 30);
            assert_eq!(request["subScene"], 2);
            assert_eq!(request["layerId"], 7);
            json!({
                "bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,
                "protocolVersion": 1,
                "worldPath": "$TestBullshit:worlds/test/arland_test.ent",
                "entities": "0x0000000000000001 {}|GenericWorldEntity|0|0",
                "hasMore": 1
            })
        });
        let root = test_root("entity-listing-numeric-has-more");
        fs::create_dir_all(&root).unwrap();
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            user_directory: Some(root.clone()),
            ..super::WorkbenchControllerOptions::default()
        });

        let page = controller
            .list_entities(None, None, Some(2), Some(7), None, 30)
            .unwrap();

        assert_eq!(page.entities.len(), 1);
        assert_eq!(page.entities[0].entity_id, "0x0000000000000001 {}");
        assert!(page.truncated);
        assert!(page.next_cursor.is_some());
        peer.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn entity_listing_rejects_a_cursor_reused_with_a_different_layer_filter() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(request["APIFunc"], "RST_WorkbenchListEntities");
            json!({
                "bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,
                "protocolVersion": 1,
                "worldPath": "$Test:worlds/test.ent",
                "entities": "0x0000000000000001 {}|TestEntity|2|7",
                "hasMore": true
            })
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            ..super::WorkbenchControllerOptions::default()
        });

        let first = controller
            .list_entities(None, None, Some(2), Some(7), None, 1)
            .unwrap();
        let failure = controller
            .list_entities(
                None,
                None,
                Some(2),
                Some(8),
                first.next_cursor.as_deref(),
                1,
            )
            .unwrap_err();

        assert_eq!(failure.code, super::WorkbenchFailureCode::Protocol);
        peer.join().unwrap();
    }

    #[test]
    fn entity_listing_sends_the_handler_sentinel_for_omitted_layer_filters() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(request["subScene"], -1);
            assert_eq!(request["layerId"], -1);
            json!({
                "bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,
                "protocolVersion": 1,
                "worldPath": "$Test:worlds/test.ent",
                "entities": "",
                "hasMore": false
            })
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            ..super::WorkbenchControllerOptions::default()
        });

        let page = controller
            .list_entities(None, None, None, None, None, 1)
            .unwrap();

        assert!(page.entities.is_empty());
        peer.join().unwrap();
    }

    #[test]
    fn entity_search_returns_component_and_match_facts_from_the_handler() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(request["APIFunc"], "RST_WorkbenchSearchEntities");
            assert_eq!(request["componentClasses"], "SCR_TriggerEntity");
            assert_eq!(request["resourceQuery"], "Checkpoints");
            json!({"bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,"protocolVersion":1,"status":"available","worldPath":"$Test:worlds/test.ent","results":"0x01|GenericEntity|0|7|{GUID}Prefabs/Checkpoints/West.et|West checkpoint|SCR_TriggerEntity,SCR_BaseGameModeComponent|name,resource,components|SCR_TriggerEntity|GenericEntity|3|||||||","totalMatches":2,"namedMatches":1,"hasMore":false})
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..Default::default()
            },
            ..Default::default()
        });
        let page = controller
            .search_entities(
                Some("checkpoint"),
                None,
                Some("Checkpoints"),
                &["SCR_TriggerEntity"],
                None,
                None,
                None,
                None,
                20,
            )
            .unwrap();
        assert_eq!(page.results.len(), 1);
        assert_eq!(
            page.results[0].entity.name.as_deref(),
            Some("West checkpoint")
        );
        assert_eq!(
            page.results[0].component_classes,
            ["SCR_TriggerEntity", "SCR_BaseGameModeComponent"]
        );
        assert_eq!(
            page.results[0].matched_component_classes,
            ["SCR_TriggerEntity"]
        );
        assert_eq!(
            page.results[0].matched_fields,
            ["name", "resource", "components"]
        );
        assert_eq!(page.summary.total_matches, 2);
        assert_eq!(page.summary.named_matches, 1);
        assert_eq!(page.summary.anonymous_matches, 1);
        peer.join().unwrap();
    }

    #[test]
    fn entity_search_returns_bounded_descendant_relation_evidence() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(request["relationDirection"], "descendant");
            assert_eq!(request["relationClassName"], "SCR_TriggerEntity");
            assert_eq!(
                request["relationComponentClasses"],
                "SCR_BaseGameModeComponent"
            );
            assert_eq!(request["relationMaxDepth"], 3);
            json!({"bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,"protocolVersion":1,"status":"available","worldPath":"$Test:worlds/test.ent","results":"0x01|GenericEntity|0|7|{GUID}Prefabs/Checkpoints/West.et|West checkpoint|SCR_TriggerEntity|relation||GenericEntity|3|descendant|2|0x02|SCR_TriggerEntity|0|7|SCR_BaseGameModeComponent","totalMatches":1,"namedMatches":1,"hasMore":false,"relationTraversalTruncated":true})
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..Default::default()
            },
            ..Default::default()
        });
        let relation = super::WorkbenchEntityRelationFilter {
            direction: super::WorkbenchEntityRelationDirection::Descendant,
            class_name: Some("SCR_TriggerEntity".to_string()),
            component_classes: vec!["SCR_BaseGameModeComponent".to_string()],
            max_depth: 3,
        };
        let page = controller
            .search_entities(None, None, None, &[], Some(&relation), None, None, None, 20)
            .unwrap();
        let matched = page.results[0].relation_match.as_ref().unwrap();
        assert_eq!(
            matched.direction,
            super::WorkbenchEntityRelationDirection::Descendant
        );
        assert_eq!(matched.depth, 2);
        assert_eq!(matched.entity_id, "0x02");
        assert_eq!(matched.class_name, "SCR_TriggerEntity");
        assert!(page.relation_traversal_truncated);
        assert_eq!(
            matched.matched_component_classes,
            ["SCR_BaseGameModeComponent"]
        );
        peer.join().unwrap();
    }

    #[test]
    fn entity_search_uses_the_authored_component_container_fallback() {
        assert!(
            super::BRIDGE_ENTITY_SEARCH_SOURCE.contains("entity.GetObjectArray(\"components\")")
        );
        assert!(super::BRIDGE_ENTITY_SEARCH_SOURCE.contains("entity.GetNumChildren()"));
        assert!(
            !super::BRIDGE_ENTITY_SEARCH_SOURCE.contains("else count = entity.GetNumChildren()")
        );
        assert!(!super::BRIDGE_ENTITY_SEARCH_SOURCE
            .contains("IEntityComponentSource.Cast(entity.GetChild(index))"));
        assert!(super::BRIDGE_ENTITY_SEARCH_SOURCE.contains("visited > 1024"));
        assert!(super::BRIDGE_ENTITY_SEARCH_SOURCE.contains("MAX_RELATION_CANDIDATES = 4096"));
        assert!(super::BRIDGE_ENTITY_SEARCH_SOURCE.contains("MAX_RESULT_CHARACTERS = 262144"));
        assert!(super::BRIDGE_ENTITY_SEARCH_SOURCE.contains("bool relationTruncated = false"));
        assert!(super::BRIDGE_ENTITY_SEARCH_SOURCE.contains("relationTraversalTruncated"));
        assert!(super::BRIDGE_ENTITY_SEARCH_SOURCE.contains("parent.GetChild(index)"));
        assert!(super::BRIDGE_ENTITY_SEARCH_SOURCE.contains("ComponentAt(entity, componentIndex)"));
        assert!(super::BRIDGE_ENTITY_SEARCH_SOURCE
            .contains("int componentCount = ComponentCount(entity);"));
        assert!(super::BRIDGE_ENTITY_SEARCH_SOURCE.contains(
            "for (int componentIndex; componentIndex < componentCount; componentIndex++)"
        ));
        assert!(super::BRIDGE_ENTITY_SEARCH_SOURCE
            .contains("components += string.Format(\"%1\", component.GetClassName())"));
    }

    #[test]
    fn entity_search_rejects_component_class_transport_delimiters() {
        let controller = super::WorkbenchController::new(Default::default());

        let failure = controller
            .search_entities(
                None,
                None,
                None,
                &["SCR_A;SCR_B"],
                None,
                None,
                None,
                None,
                1,
            )
            .unwrap_err();

        assert_eq!(failure.code, super::WorkbenchFailureCode::Protocol);
        assert!(super::valid_component_class_name(
            "SCR_MapDescriptorComponent"
        ));
        assert!(!super::valid_component_class_name("SCR_A;SCR_B"));
    }

    #[test]
    fn entity_search_rejects_unbounded_or_empty_relation_filters() {
        let controller = super::WorkbenchController::new(Default::default());
        let invalid_parent_depth = super::WorkbenchEntityRelationFilter {
            direction: super::WorkbenchEntityRelationDirection::Parent,
            class_name: Some("GenericEntity".to_string()),
            component_classes: Vec::new(),
            max_depth: 2,
        };
        let empty_descendant = super::WorkbenchEntityRelationFilter {
            direction: super::WorkbenchEntityRelationDirection::Descendant,
            class_name: None,
            component_classes: Vec::new(),
            max_depth: 3,
        };

        for relation in [&invalid_parent_depth, &empty_descendant] {
            let failure = controller
                .search_entities(None, None, None, &[], Some(relation), None, None, None, 1)
                .unwrap_err();
            assert_eq!(failure.code, super::WorkbenchFailureCode::Protocol);
        }
    }

    #[test]
    fn entity_search_cursor_is_bound_to_the_relation_filter() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(request["relationDirection"], "descendant");
            assert_eq!(request["relationClassName"], "SCR_TriggerEntity");
            json!({"bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,"protocolVersion":1,"status":"available","worldPath":"$Test:worlds/test.ent","results":"0x01|GenericEntity|0|7||||||GenericEntity|1|descendant|1|0x02|SCR_TriggerEntity|0|7|","totalMatches":2,"namedMatches":0,"hasMore":true})
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..Default::default()
            },
            ..Default::default()
        });
        let trigger_relation = super::WorkbenchEntityRelationFilter {
            direction: super::WorkbenchEntityRelationDirection::Descendant,
            class_name: Some("SCR_TriggerEntity".to_string()),
            component_classes: Vec::new(),
            max_depth: 2,
        };
        let checkpoint_relation = super::WorkbenchEntityRelationFilter {
            class_name: Some("SCR_CheckpointEntity".to_string()),
            ..trigger_relation.clone()
        };

        let page = controller
            .search_entities(
                None,
                None,
                None,
                &[],
                Some(&trigger_relation),
                None,
                None,
                None,
                1,
            )
            .unwrap();
        let failure = controller
            .search_entities(
                None,
                None,
                None,
                &[],
                Some(&checkpoint_relation),
                None,
                None,
                page.next_cursor.as_deref(),
                1,
            )
            .unwrap_err();

        assert_eq!(failure.code, super::WorkbenchFailureCode::Protocol);
        peer.join().unwrap();
    }

    #[test]
    fn entity_search_returns_a_structured_unavailable_world_status() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(request["APIFunc"], "RST_WorkbenchSearchEntities");
            json!({
                "bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,
                "protocolVersion": 1,
                "status": "world-editor-unavailable",
                "worldPath": "",
                "results": "",
                "hasMore": false
            })
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..Default::default()
            },
            ..Default::default()
        });

        let page = controller
            .search_entities(None, None, None, &[], None, None, None, None, 5)
            .unwrap();

        assert_eq!(page.status, "world-editor-unavailable");
        assert!(page.results.is_empty());
        assert!(!page.truncated);
        assert!(page.next_cursor.is_none());
        peer.join().unwrap();
    }

    #[test]
    fn entity_search_handler_initializes_its_paging_counters() {
        assert!(super::BRIDGE_ENTITY_SEARCH_SOURCE.contains("int matched = 0;"));
        assert!(super::BRIDGE_ENTITY_SEARCH_SOURCE.contains("int named = 0;"));
        assert!(super::BRIDGE_ENTITY_SEARCH_SOURCE.contains("int returned = 0;"));
        assert!(super::BRIDGE_ENTITY_SEARCH_SOURCE.contains("matched = matched + 1;"));
        assert!(super::BRIDGE_ENTITY_SEARCH_SOURCE.contains("named = named + 1;"));
        assert!(
            super::BRIDGE_ENTITY_SEARCH_SOURCE.contains("if (matched > req.offset + req.limit)")
        );
        assert!(super::BRIDGE_ENTITY_SEARCH_SOURCE.contains("response.totalMatches = matched;"));
        assert!(super::BRIDGE_ENTITY_SEARCH_SOURCE.contains("response.namedMatches = named;"));
        assert!(super::BRIDGE_ENTITY_SEARCH_SOURCE.contains("returned = returned + 1;"));
        assert!(!super::BRIDGE_ENTITY_SEARCH_SOURCE.contains("matched++"));
    }

    #[test]
    fn layer_state_reports_explicit_and_hierarchical_lock_facts() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(
                request,
                json!({"APIFunc":"RST_WorkbenchLayerState","subScene":2,"layerId":7})
            );
            json!({
                "bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,
                "protocolVersion":1,
                "status":"available",
                "subScene":2,
                "layerId":7,
                "layerPath":"Gameplay/LockedParent/Child",
                "visible":true,
                "explicitlyLocked":false,
                "lockedInHierarchy":true
            })
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            ..super::WorkbenchControllerOptions::default()
        });

        let state = controller.layer_state(2, 7).unwrap();

        assert_eq!(state.status, "available");
        assert_eq!(state.layer_path, "Gameplay/LockedParent/Child");
        assert!(state.visible);
        assert!(!state.explicitly_locked);
        assert!(state.locked_in_hierarchy);
        peer.join().unwrap();
    }

    #[test]
    fn prefab_context_normalizes_provenance_ancestors_components_and_direct_overrides() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(
                request,
                json!({"APIFunc":"RST_WorkbenchInspectPrefab","entityId":"0x01","resourceName":"","memberId":""})
            );
            json!({"bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,"protocolVersion":1,"status":"available","entity":"0x01|TestEntity|0|1","resourceName":"{GUID}Prefabs/Test.et","resourceReferenceKind":"external","contributorAddons":"BaseGame;MyAddon","ancestorResources":"{BASE_GUID}Prefabs/Base.et","prefabEditMode":true,"components":"0|MeshObject","componentProperties":"0|Enabled|bool|1|0|default;0|VisibleDistance|integer|250|1|direct;0|Offset|vector|1 2 3|1|direct;0|Scale|float|1.5|0|inherited;0|Label|string|Test|0|default","children":"0|Wheel|front-left","properties":"Mass|float|2000|1|direct;userScript|string||0|default;constructor|string||0|default;destructor|string||0|default;Name|string|Jeep|0|inherited","childCount":2})
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..Default::default()
            },
            ..Default::default()
        });
        let context = controller
            .inspect_prefab_context(Some("0x01"), None, None)
            .unwrap();
        assert_eq!(context.contributor_addons, vec!["BaseGame", "MyAddon"]);
        assert_eq!(
            context.ancestor_resources,
            vec!["{BASE_GUID}Prefabs/Base.et"]
        );
        assert!(context.prefab_edit_mode);
        assert_eq!(context.components[0].class_name, "MeshObject");
        assert_eq!(context.components[0].property_count, Some(5));
        assert_eq!(context.components[0].direct_override_count, Some(2));
        assert_eq!(context.children[0].class_name, "Wheel");
        assert_eq!(context.children[0].member_id, "member:0");
        assert_eq!(context.children[0].name.as_deref(), Some("front-left"));
        assert!(context.properties[0].directly_overridden);
        assert_eq!(
            context.properties[0].value_origin,
            Some(super::WorkbenchPrefabPropertyOrigin::Direct)
        );
        assert_eq!(context.properties.len(), 2);
        assert_eq!(context.properties[1].path, "Name");
        assert_eq!(
            context.properties[1].value_origin,
            Some(super::WorkbenchPrefabPropertyOrigin::Inherited)
        );
        assert!(!context.properties[1].directly_overridden);
        peer.join().unwrap();
    }

    #[test]
    fn prefab_bridge_reports_stored_members_without_requiring_live_entity_sources() {
        let children = &super::BRIDGE_PREFAB_SOURCE[super::BRIDGE_PREFAB_SOURCE
            .find("response.childCount = target.GetNumChildren()")
            .unwrap()..];
        assert!(children.contains("BaseContainer child = target.GetChild(i + firstChildIndex);"));
        assert!(children.contains("child.GetClassName()"));
        assert!(!children.contains("IEntitySource child = IEntitySource.Cast(target.GetChild(i))"));
    }

    #[test]
    fn prefab_component_inspection_returns_the_requested_components_full_properties() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(
                request,
                json!({"APIFunc":"RST_WorkbenchInspectPrefab","entityId":"","resourceName":"{GUID}Prefabs/Test.et","memberId":""})
            );
            json!({"bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,"protocolVersion":1,"status":"available","resourceName":"{GUID}Prefabs/Test.et","components":"0|MeshObject;1|Light","componentProperties":"0|Enabled|bool|1|0;0|Offset|vector|1 2 3|1;1|Intensity|float|500.25|0"})
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..Default::default()
            },
            ..Default::default()
        });

        let result = controller
            .inspect_prefab_component(
                None,
                Some("{GUID}Prefabs/Test.et"),
                "cmp1:0:MeshObject",
                None,
            )
            .unwrap();

        assert_eq!(result.status, "available");
        assert_eq!(result.resource_name, "{GUID}Prefabs/Test.et");
        let component = result.component.unwrap();
        assert_eq!(component.class_name, "MeshObject");
        assert_eq!(component.properties.len(), 2);
        assert_eq!(component.properties[0].path, "Enabled");
        assert_eq!(component.properties[0].value, json!(true));
        assert_eq!(
            component.properties[1].value,
            json!({"x":1.0,"y":2.0,"z":3.0})
        );
        assert!(component.properties[1].directly_overridden);
        peer.join().unwrap();
    }

    #[test]
    fn prefab_component_inspection_reports_a_missing_component_without_losing_context() {
        let (port, peer) = start_peer(
            |_| json!({"bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,"protocolVersion":1,"status":"available","resourceName":"{GUID}Prefabs/Test.et","components":"0|MeshObject"}),
        );
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..Default::default()
            },
            ..Default::default()
        });

        let result = controller
            .inspect_prefab_component(None, Some("{GUID}Prefabs/Test.et"), "cmp1:9:Missing", None)
            .unwrap();

        assert_eq!(result.status, "component-not-found");
        assert!(result.component.is_none());
        peer.join().unwrap();
    }

    #[test]
    fn prefab_edit_component_inspection_issues_component_bound_property_descriptors() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(
                request,
                json!({"APIFunc":"RST_WorkbenchInspectPrefab","entityId":"0x01 {}","resourceName":"","memberId":""})
            );
            json!({"bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,"protocolVersion":1,"status":"available","resourceName":"{GUID}Prefabs/Test.et","components":"0|MeshObject","componentProperties":"0|Enabled|bool|1|0|default"})
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..Default::default()
            },
            ..Default::default()
        });

        let result = controller
            .inspect_prefab_component(Some("0x01 {}"), None, "cmp1:0:MeshObject", None)
            .unwrap();
        let component = result.component.unwrap();
        assert!(component.properties[0]
            .write_descriptor
            .as_deref()
            .is_some_and(|value| value.starts_with("prop2:")));

        peer.join().unwrap();
    }

    #[test]
    fn prefab_context_and_component_inspection_scope_to_a_returned_member() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(
                request,
                json!({"APIFunc":"RST_WorkbenchInspectPrefab","entityId":"","resourceName":"{GUID}Prefabs/Test.et","memberId":"member:0"})
            );
            json!({"bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,"protocolVersion":1,"status":"available","resourceName":"{GUID}Prefabs/Test.et","memberId":"member:0","components":"0|MeshObject","componentProperties":"0|Enabled|bool|1|0|default","children":"0|Wheel|","properties":"coords|vector|1 2 3|1|direct","childCount":1})
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..Default::default()
            },
            ..Default::default()
        });

        let context = controller
            .inspect_prefab_context(None, Some("{GUID}Prefabs/Test.et"), Some("member:0"))
            .unwrap();

        assert_eq!(context.member_id.as_deref(), Some("member:0"));
        assert_eq!(context.children[0].class_name, "Wheel");
        assert_eq!(
            context.properties[0].value_origin,
            Some(super::WorkbenchPrefabPropertyOrigin::Direct)
        );
        peer.join().unwrap();
    }

    #[test]
    fn prefab_create_preview_issues_one_use_confirmation_bound_to_entity_and_destination() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(
                request,
                json!({"APIFunc":"RST_WorkbenchCreatePrefab","entityId":"0x01","name":"Prefabs/New.et","confirm":false})
            );
            json!({"bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,"protocolVersion":1,"status":"confirmation-required","entity":"0x01|TestEntity|0|1","destination":"Prefabs/New.et","destinationExists":false})
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..Default::default()
            },
            ..Default::default()
        });
        let preview = controller
            .create_prefab("0x01", "Prefabs/New.et", None)
            .unwrap();
        assert_eq!(preview.status, "confirmation-required");
        assert_eq!(preview.destination.as_deref(), Some("Prefabs/New.et"));
        assert_eq!(preview.destination_exists, Some(false));
        assert!(preview.confirmation_token.is_some());
        peer.join().unwrap();
    }

    #[test]
    fn generic_prefab_create_preview_uses_the_native_workbench_path() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(
                request,
                json!({
                    "APIFunc":"RST_WorkbenchCreateGenericPrefab",
                    "name":"Prefabs/Generated/RST_Test.et",
                    "confirm":false
                })
            );
            json!({
                "bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,
                "protocolVersion":1,
                "status":"confirmation-required",
                "destination":"Prefabs/Generated/RST_Test.et",
                "destinationExists":false
            })
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..Default::default()
            },
            ..Default::default()
        });

        let preview = controller
            .create_generic_prefab("Prefabs/Generated/RST_Test.et", None)
            .unwrap();

        assert_eq!(preview.status, "confirmation-required");
        assert_eq!(
            preview.destination.as_deref(),
            Some("Prefabs/Generated/RST_Test.et")
        );
        assert!(preview.confirmation_token.is_some());
        peer.join().unwrap();
    }

    #[test]
    fn prefab_resource_save_preview_is_bound_to_the_exact_resource() {
        let resource_name = "{1234567890ABCDEF}Prefabs/Test_GenericEntity.et";
        let (port, peer) = start_peer(move |request| {
            assert_eq!(
                request,
                json!({
                    "APIFunc":"RST_WorkbenchSavePrefab",
                    "entityId":"",
                    "resourceName":resource_name,
                    "confirm":false
                })
            );
            json!({
                "bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,
                "protocolVersion":1,
                "status":"confirmation-required",
                "destinationExists":0
            })
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..Default::default()
            },
            ..Default::default()
        });

        let preview = controller
            .save_prefab(None, Some(resource_name), None)
            .unwrap();

        assert_eq!(preview.status, "confirmation-required");
        assert_eq!(preview.destination_exists, Some(false));
        assert!(preview.confirmation_token.is_some());
        peer.join().unwrap();
    }

    #[test]
    fn prefab_resource_component_add_preview_is_bound_to_resource_and_class() {
        let resource_name = "{1234567890ABCDEF}Prefabs/Test_GenericEntity.et";
        let (port, peer) = start_peer(move |request| {
            assert_eq!(
                request,
                json!({
                    "APIFunc":"RST_WorkbenchAddPrefabResourceComponent",
                    "resourceName":resource_name,
                    "className":"ScriptComponent",
                    "confirm":false
                })
            );
            json!({
                "bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,
                "protocolVersion":1,
                "status":"confirmation-required",
                "resourceName":resource_name,
                "componentIndex":-1,
                "templateSaved":0
            })
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..Default::default()
            },
            ..Default::default()
        });

        let preview = controller
            .add_prefab_resource_component(resource_name, "ScriptComponent", None)
            .unwrap();

        assert_eq!(preview.status, "confirmation-required");
        assert_eq!(preview.persistence_path, "workbench-resource");
        assert!(!preview.template_saved);
        assert!(preview.confirmation_token.is_some());
        peer.join().unwrap();
    }

    #[test]
    fn prefab_resource_component_remove_preview_is_bound_to_inspected_identity() {
        let resource_name = "{1234567890ABCDEF}Prefabs/Test_GenericEntity.et";
        let (port, peer) = start_peer(move |request| {
            assert_eq!(
                request,
                json!({
                    "APIFunc":"RST_WorkbenchRemovePrefabResourceComponent",
                    "resourceName":resource_name,
                    "className":"ScriptComponent",
                    "componentIndex":0,
                    "confirm":false
                })
            );
            json!({
                "bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,
                "protocolVersion":1,
                "status":"confirmation-required",
                "resourceName":resource_name,
                "componentIndex":0,
                "componentClass":"ScriptComponent",
                "templateSaved":0
            })
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..Default::default()
            },
            ..Default::default()
        });

        let preview = controller
            .remove_prefab_resource_component(resource_name, "cmp1:0:ScriptComponent", None)
            .unwrap();

        assert_eq!(preview.status, "confirmation-required");
        assert_eq!(
            preview.component_id.as_deref(),
            Some("cmp1:0:ScriptComponent")
        );
        assert!(preview.confirmation_token.is_some());
        peer.join().unwrap();
    }

    #[test]
    fn prefab_resource_property_preview_is_bound_to_resource_descriptor_and_value() {
        let resource_name = "{1234567890ABCDEF}Prefabs/Test_GenericEntity.et";
        let (port, peer) = start_peer(move |request| {
            assert_eq!(
                request,
                json!({
                    "APIFunc":"RST_WorkbenchSetPrefabResourceProperty",
                    "resourceName":resource_name,
                    "componentIndex":-1,
                    "componentClass":"",
                    "propertyName":"scale",
                    "expectedValue":"1",
                    "value":"2",
                    "confirm":false
                })
            );
            json!({
                "bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,
                "protocolVersion":1,
                "status":"confirmation-required",
                "resourceName":resource_name,
                "templateSaved":0
            })
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..Default::default()
            },
            ..Default::default()
        });
        controller
            .property_write_descriptors
            .lock()
            .unwrap()
            .insert(
                "prop2:resource".to_string(),
                super::PropertyWriteDescriptor {
                    entity_id: resource_name.to_string(),
                    component_id: None,
                    property_name: "scale".to_string(),
                    data_type: "float".to_string(),
                    observed_value: "1".to_string(),
                    issued: Instant::now(),
                },
            );

        let preview = controller
            .set_prefab_resource_property(resource_name, None, "prop2:resource", json!(2.0), None)
            .unwrap();

        assert_eq!(preview.status, "confirmation-required");
        assert!(!preview.template_saved);
        assert!(preview.confirmation_token.is_some());
        peer.join().unwrap();
    }

    #[test]
    fn viewport_context_keeps_compact_cursor_result_separate_from_optional_ray_diagnostics() {
        let response = || {
            json!({
                "bridgeVersion": super::WORKBENCH_BRIDGE_VERSION, "protocolVersion":1, "status":"available",
                "width":1920, "height":1080, "mouseX":960, "mouseY":540, "mouseInside":true,
                "cameraX":10.0, "cameraY":20.0, "cameraZ":30.0,
                "cameraDirectionX":0.0, "cameraDirectionY":0.0, "cameraDirectionZ":1.0,
                "startX":10.0, "startY":20.0, "startZ":30.0,
                "endX":100.0, "endY":40.0, "endZ":300.0,
                "directionX":0.3, "directionY":-0.1, "directionZ":0.9
            })
        };
        let (port, peer) = start_peer(move |request| {
            assert_eq!(request, json!({"APIFunc":"RST_WorkbenchViewportContext"}));
            response()
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..Default::default()
            },
            ..Default::default()
        });
        let compact = controller
            .viewport_context(super::WorkbenchViewportContextOptions { include_ray: false })
            .unwrap();
        assert_eq!(compact.mouse_world_position.unwrap().x, 100.0);
        assert!(compact.width.is_none());
        assert!(compact.ray_start.is_none());
        peer.join().unwrap();

        let (port, peer) = start_peer(move |_| response());
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..Default::default()
            },
            ..Default::default()
        });
        let detailed = controller
            .viewport_context(super::WorkbenchViewportContextOptions { include_ray: true })
            .unwrap();
        assert_eq!(detailed.width, Some(1920));
        assert_eq!(detailed.camera_direction.unwrap().z, 1.0);
        assert_eq!(detailed.ray_end.unwrap().z, 300.0);
        peer.join().unwrap();
    }

    #[test]
    fn trace_serializes_bounded_policy_and_normalizes_distance_and_ocean_hit() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(
                request,
                json!({
                    "APIFunc":"RST_WorkbenchTrace", "startX":0.0, "startY":10.0, "startZ":0.0,
                    "endX":0.0, "endY":-10.0, "endZ":0.0, "shape":"sphere", "radius":2.0,
                    "minsX":0.0, "minsY":0.0, "minsZ":0.0, "maxsX":0.0, "maxsY":0.0, "maxsZ":0.0,
                    "entities":false, "terrain":false, "ocean":true, "targetLayers":0
                })
            );
            json!({
                "bridgeVersion": super::WORKBENCH_BRIDGE_VERSION, "protocolVersion":1, "status":"available", "hit":true,
                "fraction":0.5, "distance":10.0, "hitX":0.0, "hitY":0.0, "hitZ":0.0,
                "normalX":0.0, "normalY":1.0, "normalZ":0.0, "kind":"ocean"
            })
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..Default::default()
            },
            ..Default::default()
        });
        let hit = controller
            .trace(super::WorkbenchTraceOptions {
                start: super::WorkbenchEntityPosition {
                    x: 0.0,
                    y: 10.0,
                    z: 0.0,
                },
                end: super::WorkbenchEntityPosition {
                    x: 0.0,
                    y: -10.0,
                    z: 0.0,
                },
                shape: super::WorkbenchTraceShape::Sphere,
                radius: Some(2.0),
                box_mins: None,
                box_maxs: None,
                entities: false,
                terrain: false,
                ocean: true,
                target_layers: None,
            })
            .unwrap();
        assert_eq!(hit.distance, Some(10.0));
        assert_eq!(hit.kind, Some(super::WorkbenchTraceHitKind::Ocean));
        peer.join().unwrap();
    }

    #[test]
    fn trace_rejects_unbounded_or_invalid_sweep_requests_before_gateway_dispatch() {
        let controller = super::WorkbenchController::new(Default::default());
        let valid_position = super::WorkbenchEntityPosition {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        };
        for options in [
            super::WorkbenchTraceOptions {
                start: valid_position.clone(),
                end: super::WorkbenchEntityPosition {
                    x: 10_001.0,
                    y: 0.0,
                    z: 0.0,
                },
                shape: super::WorkbenchTraceShape::Line,
                radius: None,
                box_mins: None,
                box_maxs: None,
                entities: true,
                terrain: false,
                ocean: false,
                target_layers: None,
            },
            super::WorkbenchTraceOptions {
                start: valid_position.clone(),
                end: valid_position.clone(),
                shape: super::WorkbenchTraceShape::Box,
                radius: None,
                box_mins: Some(valid_position.clone()),
                box_maxs: Some(valid_position.clone()),
                entities: true,
                terrain: false,
                ocean: false,
                target_layers: None,
            },
            super::WorkbenchTraceOptions {
                start: valid_position.clone(),
                end: valid_position.clone(),
                shape: super::WorkbenchTraceShape::Line,
                radius: None,
                box_mins: None,
                box_maxs: None,
                entities: false,
                terrain: true,
                ocean: false,
                target_layers: Some(1),
            },
        ] {
            assert_eq!(
                controller.trace(options).unwrap_err().code,
                super::WorkbenchFailureCode::Protocol
            );
        }
    }

    #[test]
    fn terrain_sampling_returns_bounded_grid_metadata_and_derived_summary() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(
                request,
                json!({
                    "APIFunc": "RST_WorkbenchSampleTerrain",
                    "centerX": 100.0,
                    "centerZ": 200.0,
                    "halfExtentMeters": 30.0,
                    "spacingMeters": 1.0,
                    "includeWater": true
                })
            );
            json!({
                "bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,
                "protocolVersion": 1,
                "status": "available",
                "centerX": 100.0,
                "centerZ": 200.0,
                "halfExtentMeters": 30.0,
                "requestedSpacingMeters": 1.0,
                "effectiveSpacingMeters": 10.0,
                "spacingClamped": true,
                "gridOriginX": 70.0,
                "gridOriginZ": 170.0,
                "gridWidth": 2,
                "gridHeight": 2,
                "heights": "10;20;~;30",
                "waterTypes": "n;p;~;r",
                "waterSurfaceHeights": "~;22;~;35",
                "waterDepthsAboveTerrain": "~;2;~;5",
                "boundsMinX": 0.0,
                "boundsMinY": -5.0,
                "boundsMinZ": 0.0,
                "boundsMaxX": 10240.0,
                "boundsMaxY": 500.0,
                "boundsMaxZ": 10240.0,
                "heightmapResolutionX": 1024,
                "heightmapResolutionZ": 1024,
                "nativeSpacingMeters": 10.0,
                "tileCountX": 8,
                "tileCountZ": 8
            })
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            ..super::WorkbenchControllerOptions::default()
        });

        let sample = controller
            .sample_terrain(super::WorkbenchTerrainSampleOptions {
                center_x: 100.0,
                center_z: 200.0,
                half_extent_meters: 30.0,
                spacing_meters: Some(1.0),
                include_water: true,
            })
            .unwrap();

        assert_eq!(sample.status, "available");
        let terrain = sample.terrain.expect("terrain metadata");
        assert_eq!(terrain.native_spacing_meters, 10.0);
        assert_eq!(terrain.heightmap_resolution_x, 1024);
        let grid = sample.grid.expect("terrain grid");
        assert!(grid.spacing_clamped);
        assert_eq!(grid.effective_spacing_meters, 10.0);
        assert_eq!(grid.heights, vec![Some(10.0), Some(20.0), None, Some(30.0)]);
        let summary = sample.summary.expect("terrain summary");
        assert_eq!(summary.valid_sample_count, 3);
        assert_eq!(summary.minimum_height, Some(10.0));
        assert_eq!(summary.maximum_height, Some(30.0));
        assert_eq!(summary.mean_height, Some(20.0));
        assert_eq!(summary.elevation_range, Some(20.0));
        assert_eq!(summary.steepest_adjacent_slope_degrees, Some(45.0));
        let water = sample.water.expect("water grid");
        assert_eq!(
            water.types,
            vec![
                Some(super::WorkbenchTerrainWaterType::None),
                Some(super::WorkbenchTerrainWaterType::Pond),
                None,
                Some(super::WorkbenchTerrainWaterType::River),
            ]
        );
        let water_summary = sample.water_summary.expect("water summary");
        assert_eq!(water_summary.wet_sample_count, 2);
        assert_eq!(water_summary.pond_sample_count, 1);
        assert_eq!(water_summary.river_sample_count, 1);
        assert_eq!(water_summary.maximum_depth_above_terrain, Some(5.0));
        peer.join().unwrap();
    }

    #[test]
    fn terrain_sampling_rejects_invalid_parameter_values_before_gateway_dispatch() {
        let controller = super::WorkbenchController::new(Default::default());
        for options in [
            super::WorkbenchTerrainSampleOptions {
                center_x: f32::NAN,
                center_z: 0.0,
                half_extent_meters: 30.0,
                spacing_meters: None,
                include_water: false,
            },
            super::WorkbenchTerrainSampleOptions {
                center_x: 0.0,
                center_z: 0.0,
                half_extent_meters: 0.0,
                spacing_meters: None,
                include_water: false,
            },
            super::WorkbenchTerrainSampleOptions {
                center_x: 0.0,
                center_z: 0.0,
                half_extent_meters: 30.0,
                spacing_meters: Some(501.0),
                include_water: false,
            },
        ] {
            assert_eq!(
                controller.sample_terrain(options).unwrap_err().code,
                super::WorkbenchFailureCode::Protocol
            );
        }
    }

    #[test]
    fn terrain_sampling_requires_the_matching_managed_handler_version() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(
                request,
                json!({
                    "APIFunc": "RST_WorkbenchSampleTerrain",
                    "centerX": 0.0,
                    "centerZ": 0.0,
                    "halfExtentMeters": 30.0,
                    "spacingMeters": 0.0,
                    "includeWater": false
                })
            );
            json!({"bridgeVersion":"1.19.0","protocolVersion":1,"status":"terrain-unavailable"})
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            ..super::WorkbenchControllerOptions::default()
        });

        assert_eq!(
            controller
                .sample_terrain(super::WorkbenchTerrainSampleOptions {
                    center_x: 0.0,
                    center_z: 0.0,
                    half_extent_meters: 30.0,
                    spacing_meters: None,
                    include_water: false,
                })
                .unwrap_err()
                .code,
            super::WorkbenchFailureCode::Protocol
        );
        peer.join().unwrap();
    }

    #[test]
    fn terrain_handler_aligns_coarser_spacing_to_native_lattice_and_enforces_limits() {
        assert!(super::BRIDGE_TERRAIN_SAMPLE_SOURCE.contains("typedRequest.spacingMeters > 500"));
        assert!(super::BRIDGE_TERRAIN_SAMPLE_SOURCE.contains(
            "Math.Ceil(typedRequest.spacingMeters / response.nativeSpacingMeters) * response.nativeSpacingMeters"
        ));
    }

    #[test]
    fn clear_selection_accepts_workbenchs_numeric_boolean_fields() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(request, json!({"APIFunc": "RST_WorkbenchClearSelection"}));
            json!({
                "bridgeVersion": "1.10.0",
                "protocolVersion": 1,
                "editorAvailable": 1,
                "status": "available",
                "selectedCount": 0,
                "selectedEntities": "",
                "selectedEntitiesTruncated": 0
            })
        });
        let root = test_root("clear-selection-numeric-booleans");
        fs::create_dir_all(&root).unwrap();
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            user_directory: Some(root.clone()),
            ..super::WorkbenchControllerOptions::default()
        });

        let selection = controller.clear_selection().unwrap();

        assert!(selection.editor_available);
        assert_eq!(selection.selected_count, 0);
        assert!(selection.selected_entities.is_empty());
        assert!(!selection.selected_entities_truncated);
        peer.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn set_selection_uses_one_exact_entity_identity() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(
                request,
                json!({
                    "APIFunc": "RST_WorkbenchSetSelection",
                    "entityId": "0x0000000000000003 {}"
                })
            );
            json!({
                "bridgeVersion": "1.11.0",
                "protocolVersion": 1,
                "status": "selected",
                "entity": "0x0000000000000003 {}|GenericTerrainEntity|0|0|10|20|30"
            })
        });
        let root = test_root("set-selection-single-entity");
        fs::create_dir_all(&root).unwrap();
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            user_directory: Some(root.clone()),
            ..super::WorkbenchControllerOptions::default()
        });

        let result = controller.set_selection("0x0000000000000003 {}").unwrap();

        assert_eq!(result.status, "selected");
        assert_eq!(result.entity.unwrap().class_name, "GenericTerrainEntity");
        peer.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn create_entity_uses_explicit_resource_position_angles_and_layer() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(
                request,
                json!({"APIFunc":"RST_WorkbenchCreateEntity","resourceName":"{GUID}Prefabs/Test.et","targetIsResource":true,"subScene":2,"x":1.0,"y":2.0,"z":3.0,"pitch":4.0,"yaw":5.0,"roll":6.0,"layerId":7,"name":"Created"})
            );
            json!({"bridgeVersion":"1.17.0","protocolVersion":1,"status":"created","entity":"0x01 {}|TestEntity|0|7|1|2|3"})
        });
        let root = test_root("create-entity");
        fs::create_dir_all(&root).unwrap();
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            user_directory: Some(root.clone()),
            ..super::WorkbenchControllerOptions::default()
        });
        let result = controller
            .create_entity(super::WorkbenchCreateEntityOptions {
                target: "{GUID}Prefabs/Test.et".to_string(),
                target_is_resource: true,
                sub_scene: 2,
                position: super::WorkbenchEntityPosition {
                    x: 1.0,
                    y: 2.0,
                    z: 3.0,
                },
                angles: super::WorkbenchEntityPosition {
                    x: 4.0,
                    y: 5.0,
                    z: 6.0,
                },
                layer_id: 7,
                name: Some("Created".to_string()),
            })
            .unwrap();
        assert_eq!(result.status, "created");
        assert_eq!(result.entity.unwrap().layer_id, 7);
        peer.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn create_entity_bridge_distinguishes_resource_loading_from_editor_rejection() {
        assert!(super::BRIDGE_ENTITY_MUTATION_SOURCE
            .contains("response.status = \"resource-load-failed\""));
        let missing_entity = super::BRIDGE_ENTITY_MUTATION_SOURCE
            .find("if (!entity)")
            .unwrap();
        let rejected = super::BRIDGE_ENTITY_MUTATION_SOURCE
            .find("response.status = \"create-rejected\"")
            .unwrap();
        assert!(missing_entity < rejected);
    }

    #[test]
    fn entity_mutations_resolve_the_exact_selected_entity_before_the_general_enumeration() {
        assert!(super::BRIDGE_ENTITY_MUTATION_SOURCE.contains("api.GetSelectedEntitiesCount()"));
        assert!(super::BRIDGE_ENTITY_MUTATION_SOURCE.contains("api.GetSelectedEntity(i)"));
        assert!(super::BRIDGE_ENTITY_MUTATION_SOURCE.contains("api.GetEditorEntityCount()"));
        assert!(super::BRIDGE_ENTITY_MUTATION_SOURCE.contains("RegV(\"entityId\")"));
    }

    #[test]
    fn atomic_entity_transform_sets_all_components_and_returns_readback() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(request["APIFunc"], "RST_WorkbenchTransformEntity");
            assert_eq!(request["entityId"], "0x01 {}");
            assert_eq!(request["scale"], 2.0);
            json!({
                "bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,
                "protocolVersion":1,
                "status":"transformed",
                "entity":"0x01 {}|TestEntity|0|7|10|20|30",
                "position":"10 20 30",
                "angles":"4 5 6",
                "scale":2.0
            })
        });
        let root = test_root("transform-entity");
        fs::create_dir_all(&root).unwrap();
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            user_directory: Some(root.clone()),
            ..super::WorkbenchControllerOptions::default()
        });
        let result = controller
            .transform_entity(
                "0x01 {}",
                super::WorkbenchEntityTransform {
                    position: super::WorkbenchEntityPosition {
                        x: 10.0,
                        y: 20.0,
                        z: 30.0,
                    },
                    angles: super::WorkbenchEntityPosition {
                        x: 4.0,
                        y: 5.0,
                        z: 6.0,
                    },
                    scale: 2.0,
                },
            )
            .unwrap();
        assert_eq!(result.status, "transformed");
        assert_eq!(result.transform.unwrap().scale, 2.0);
        peer.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn history_results_preserve_native_availability_and_change_facts() {
        let (port, peer) = start_peer_sequence(vec![
            (
                json!({"APIFunc":"RST_WorkbenchUndo"}),
                json!({"bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,"protocolVersion":1,"operation":"undo","status":"invoked","historyAvailable":true,"changed":true}),
            ),
            (
                json!({"APIFunc":"RST_WorkbenchRedo"}),
                json!({"bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,"protocolVersion":1,"operation":"redo","status":"invoked","historyAvailable":true,"changed":true}),
            ),
        ]);
        let root = test_root("history-operations");
        fs::create_dir_all(&root).unwrap();
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            user_directory: Some(root.clone()),
            ..super::WorkbenchControllerOptions::default()
        });
        assert!(controller.undo().unwrap().history_available);
        assert!(controller.redo().unwrap().changed);
        peer.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn history_bridge_invokes_the_live_game_world_editor_route() {
        assert!(super::BRIDGE_HISTORY_SOURCE.contains("Workbench.GetModule(WorldEditor)"));
        assert!(
            super::BRIDGE_HISTORY_SOURCE.contains("array<string> menuPath = {\"Edit\", \"Undo\"}")
        );
        assert!(
            super::BRIDGE_HISTORY_SOURCE.contains("array<string> menuPath = {\"Edit\", \"Redo\"}")
        );
        assert!(super::BRIDGE_HISTORY_SOURCE.contains("worldEditor.ExecuteAction(menuPath, true)"));
        assert!(super::BRIDGE_HISTORY_SOURCE.contains("response.status = \"history-unavailable\""));
        assert!(
            super::BRIDGE_HISTORY_SOURCE.contains("response.status = \"world-editor-unavailable\"")
        );
    }

    #[test]
    fn history_results_report_rejected_actions_without_claiming_a_change() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(request, json!({"APIFunc":"RST_WorkbenchUndo"}));
            json!({"bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,"protocolVersion":1,"operation":"undo","status":"history-unavailable","historyAvailable":false,"changed":false})
        });
        let root = test_root("history-unavailable");
        fs::create_dir_all(&root).unwrap();
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            user_directory: Some(root.clone()),
            ..super::WorkbenchControllerOptions::default()
        });
        let result = controller.undo().unwrap();
        assert_eq!(result.status, "history-unavailable");
        assert!(!result.history_available);
        assert!(!result.changed);
        peer.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn entity_transform_and_parenting_use_exact_id_requests() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(
                request,
                json!({"APIFunc":"RST_WorkbenchMoveEntity","entityId":"0x01 {}","x":10.0,"y":20.0,"z":30.0})
            );
            json!({"bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,"protocolVersion":1,"status":"moved","entity":"0x01 {}|TestEntity|0|7|10|20|30"})
        });
        let root = test_root("move-entity");
        fs::create_dir_all(&root).unwrap();
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            user_directory: Some(root.clone()),
            ..super::WorkbenchControllerOptions::default()
        });
        let result = controller
            .move_entity(
                "0x01 {}",
                super::WorkbenchEntityPosition {
                    x: 10.0,
                    y: 20.0,
                    z: 30.0,
                },
            )
            .unwrap();
        assert_eq!(result.status, "moved");
        assert_eq!(result.entity.unwrap().position.unwrap().x, 10.0);
        peer.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn duplicate_entity_uses_native_clone_with_an_explicit_destination() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(
                request,
                json!({"APIFunc":"RST_WorkbenchDuplicateEntity","entityId":"0x01 {}","x":11.0,"y":22.0,"z":33.0,"name":"Copy"})
            );
            json!({"bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,"protocolVersion":1,"status":"duplicated","entity":"0x02 {}|TestEntity|0|7|11|22|33"})
        });
        let root = test_root("duplicate-entity");
        fs::create_dir_all(&root).unwrap();
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            user_directory: Some(root.clone()),
            ..super::WorkbenchControllerOptions::default()
        });
        let result = controller
            .duplicate_entity(
                "0x01 {}",
                super::WorkbenchEntityPosition {
                    x: 11.0,
                    y: 22.0,
                    z: 33.0,
                },
                Some("Copy"),
            )
            .unwrap();
        assert_eq!(result.entity.unwrap().entity_id, "0x02 {}");
        peer.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn delete_entity_preview_returns_a_one_use_confirmation_token() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(
                request,
                json!({"APIFunc":"RST_WorkbenchDeleteEntity","entityId":"0x01 {}","confirm":false})
            );
            json!({"bridgeVersion":"1.17.0","protocolVersion":1,"status":"confirmation-required","entity":"0x01 {}|TestEntity|0|7|1|2|3"})
        });
        let root = test_root("delete-entity-preview");
        fs::create_dir_all(&root).unwrap();
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            user_directory: Some(root.clone()),
            ..super::WorkbenchControllerOptions::default()
        });
        let result = controller.delete_entity("0x01 {}", None).unwrap();
        assert_eq!(result.status, "confirmation-required");
        assert!(result.confirmation_token.is_some());
        peer.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn entity_inspection_accepts_workbenchs_numeric_hierarchy_flags() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(request["APIFunc"], "RST_WorkbenchInspectEntity");
            json!({
                "bridgeVersion": "1.11.0",
                "protocolVersion": 1,
                "editorAvailable": 1,
                "status": "available",
                "entity": "0x0000000000000003 {}|GenericTerrainEntity|0|0|1|2|3",
                "resourceReferenceKind": "world-subscene",
                "resourceName": "$Example:worlds/Test.ent",
                "contributorAddons": "Example",
                "contributorAddonsTruncated": 0,
                "ancestors": "",
                "ancestorsTruncated": 0,
                "children": "",
                "childrenTruncated": 0,
                "components": "0|GRAY_TEST",
                "componentProperties": "0|Enabled|bool|1|0;0|m_iNumTest|integer|99|1",
                "properties": "coords|vector|1 2 3|1;scale|float|1|0;m_bTestBool|bool|1|0;m_iNumTest|integer|101|1",
                "propertiesTruncated": 0
            })
        });
        let root = test_root("entity-inspection-numeric-hierarchy-flags");
        fs::create_dir_all(&root).unwrap();
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            user_directory: Some(root.clone()),
            ..super::WorkbenchControllerOptions::default()
        });

        let inspection = controller.inspect_entity("0x0000000000000003 {}").unwrap();

        assert!(inspection.editor_available);
        let entity = inspection.entity.unwrap();
        assert_eq!(entity.class_name, "GenericTerrainEntity");
        assert_eq!(
            entity.position,
            Some(super::WorkbenchEntityPosition {
                x: 1.0,
                y: 2.0,
                z: 3.0,
            })
        );
        assert!(!inspection.ancestors_truncated);
        assert!(!inspection.children_truncated);
        assert_eq!(inspection.resource_reference_kind, "world-subscene");
        assert_eq!(
            inspection.resource_name.as_deref(),
            Some("$Example:worlds/Test.ent")
        );
        assert_eq!(inspection.contributor_addons, vec!["Example"]);
        assert!(!inspection.contributor_addons_truncated);
        assert_eq!(inspection.components.len(), 1);
        assert_eq!(inspection.components[0].class_name, "GRAY_TEST");
        assert_eq!(inspection.components[0].property_count, Some(2));
        assert_eq!(inspection.components[0].direct_override_count, Some(1));
        assert_eq!(
            inspection.properties[0].value,
            json!({"x": 1.0, "y": 2.0, "z": 3.0})
        );
        assert_eq!(inspection.properties[1].value, json!(1.0));
        assert_eq!(inspection.properties[2].value, json!(101));
        assert!(inspection.properties[2].directly_overridden);
        assert!(inspection
            .properties
            .iter()
            .all(|property| property.path != "m_bTestBool"));
        assert!(!inspection.properties_truncated);
        peer.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn entity_inspection_bridge_uses_the_component_container_fallback() {
        assert!(super::BRIDGE_ENTITY_INSPECT_SOURCE
            .contains("int componentCount = ComponentCount(target);"));
        assert!(
            super::BRIDGE_ENTITY_INSPECT_SOURCE.contains("entity.GetObjectArray(\"components\")")
        );
    }

    #[test]
    fn resource_inspection_handles_resources_without_configurations() {
        assert!(super::BRIDGE_INSPECT_RESOURCE_SOURCE.contains(
            "ref BaseContainerList configurations = meta.GetObjectArray(\"Configurations\");"
        ));
        assert!(super::BRIDGE_INSPECT_RESOURCE_SOURCE.contains("configurations.Count() == 0"));
        assert!(super::BRIDGE_INSPECT_RESOURCE_SOURCE.contains("configurations.Get(0)"));
    }

    #[test]
    fn play_session_preserves_direct_observations_without_claiming_runtime_certainty() {
        assert_eq!(
            super::play_session(&Some("editing".to_string()), true, true),
            Some(super::WorkbenchPlaySession::Unknown)
        );
        assert_eq!(
            super::play_session(&None, true, true),
            Some(super::WorkbenchPlaySession::Unknown)
        );
        assert_eq!(
            super::play_session(&Some("likely-running".to_string()), true, false),
            Some(super::WorkbenchPlaySession::LikelyRunning)
        );
        assert_eq!(
            super::play_session(&None, false, false),
            Some(super::WorkbenchPlaySession::Unavailable)
        );
        assert_eq!(
            super::play_session(&Some("running".to_string()), true, false),
            None
        );
    }

    #[test]
    fn play_session_accepts_workbenchs_numeric_boolean_response() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(
                request,
                json!({
                    "APIFunc": "RST_WorkbenchPlaySession",
                    "start": true,
                    "debugMode": false,
                    "fullScreen": false
                })
            );
            json!({
                "accepted": 1,
                "status": "play-started"
            })
        });
        let root = test_root("play-session-numeric-boolean");
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            user_directory: Some(root.clone()),
            ..super::WorkbenchControllerOptions::default()
        });

        let result = controller.set_play_session(true, false, false).unwrap();

        assert!(result.accepted);
        assert_eq!(result.status, "play-started");
        peer.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_status_uses_the_documented_net_api_transaction() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(request, json!({"APIFunc": "IsWorkbenchRunning"}));
            json!({"IsRunning": true, "ScriptsCompiled": false})
        });
        let gateway = test_gateway(port);

        assert_eq!(
            gateway.status().unwrap(),
            WorkbenchStatus {
                is_running: true,
                scripts_compiled: false,
            }
        );
        peer.join().unwrap();
    }

    #[test]
    fn state_uses_its_typed_handler_without_profile_maintenance() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(request, json!({"APIFunc": "RST_WorkbenchState"}));
            json!({
                "bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,
                "protocolVersion": 1,
                "mode": "workbench",
                "playSession": "unavailable",
                "loadedAddons": "ArmaReforger"
            })
        });
        let root = test_root("state-net-only");
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            user_directory: Some(root.clone()),
            ..super::WorkbenchControllerOptions::default()
        });
        let bridge_file = controller
            .paths()
            .bridge_directory
            .join("RST_WorkbenchState.c");
        controller
            .write_managed_files(&controller.paths().bridge_directory)
            .unwrap();
        fs::write(&bridge_file, "stale-state-handler").unwrap();

        assert_eq!(controller.state().unwrap().mode, "workbench");
        assert_eq!(
            fs::read_to_string(bridge_file).unwrap(),
            "stale-state-handler"
        );
        peer.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn state_exposes_the_authoritative_active_world_path() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(request, json!({"APIFunc": "RST_WorkbenchState"}));
            json!({
                "bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,
                "protocolVersion": 1,
                "mode": "world-editor",
                "worldEditorActive": true,
                "worldEditorModulePresent": true,
                "worldEditorApiAvailable": true,
                "playSession": "unknown",
                "loadedAddons": "McpFixture",
                "activeWorldPath": "McpFixture/Worlds/Conformance.ent"
            })
        });
        let root = test_root("state-active-world");
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            user_directory: Some(root.clone()),
            ..super::WorkbenchControllerOptions::default()
        });

        assert_eq!(
            controller.state().unwrap().active_world_path.as_deref(),
            Some("McpFixture/Worlds/Conformance.ent")
        );
        peer.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn validation_uses_native_compiler_without_profile_maintenance() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(
                request,
                json!({"APIFunc": "ValidateScripts", "Configuration": "WORKBENCH"})
            );
            json!({"Success": true, "Errors": [], "Warnings": []})
        });
        let root = test_root("validation-net-only");
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            user_directory: Some(root.clone()),
            ..super::WorkbenchControllerOptions::default()
        });
        let bridge_file = controller
            .paths()
            .bridge_directory
            .join("RST_WorkbenchState.c");
        controller
            .write_managed_files(&controller.paths().bridge_directory)
            .unwrap();
        fs::write(&bridge_file, "stale-validation-handler").unwrap();

        assert!(controller.validate_scripts().unwrap().success);
        assert_eq!(
            fs::read_to_string(bridge_file).unwrap(),
            "stale-validation-handler"
        );
        peer.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn save_actions_use_only_the_typed_handler() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(
                request,
                json!({"APIFunc": "RST_WorkbenchState", "executeSaveAllAction": true})
            );
            json!({
                "bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,
                "protocolVersion": 1,
                "saveAllActionAccepted": true,
                "saveAllActionPath": "File/Save All",
                "worldSaveActionAccepted": false,
                "worldSaveStatus": "skipped-no-open-world"
            })
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            ..super::WorkbenchControllerOptions::default()
        });

        assert!(controller.save().unwrap().save_all_accepted);
        peer.join().unwrap();
    }

    #[test]
    fn native_validation_uses_the_fixed_workbench_profile_and_normalizes_duplicates() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(
                request,
                json!({"APIFunc": "ValidateScripts", "Configuration": "WORKBENCH"})
            );
            json!({
                "Success": false,
                "Errors": [
                    {"error": "broken", "file": "scripts/A.c", "fileAbs": "C:\\Addon\\scripts\\A.c", "addon": "Addon", "line": 7},
                    {"error": "broken", "file": "scripts/A.c", "fileAbs": "C:\\Addon\\scripts\\A.c", "addon": "Addon", "line": 7}
                ],
                "Warnings": [
                    {"error": "broken", "file": "scripts/A.c", "fileAbs": "C:\\Addon\\scripts\\A.c", "addon": "Addon", "line": 7},
                    {"error": "unused", "file": "scripts/B.c", "line": 4}
                ]
            })
        });
        let gateway = super::WorkbenchGateway::with_host(
            super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                validation_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            wine_test_host(),
        );

        let result = gateway.validate_scripts().unwrap();

        assert!(!result.success);
        assert_eq!(result.profile, "WORKBENCH");
        assert_eq!(result.diagnostics.len(), 2);
        assert_eq!(
            result.diagnostics[0].severity,
            WorkbenchDiagnosticSeverity::Error
        );
        assert_eq!(
            result.diagnostics[0].location,
            WorkbenchDiagnosticLocation {
                file: "scripts/A.c".to_string(),
                file_abs: Some("/prefix/drive_c/Addon/scripts/A.c".into()),
                addon: Some("Addon".to_string()),
                line: 7,
            }
        );
        assert_eq!(
            result.diagnostics[1].severity,
            WorkbenchDiagnosticSeverity::Warning
        );
        peer.join().unwrap();
    }

    #[test]
    fn malformed_native_status_is_a_protocol_failure() {
        let (port, peer) = start_peer(|_| json!({"IsRunning": "yes"}));
        let gateway = test_gateway(port);

        assert_eq!(
            gateway.status().unwrap_err().code,
            WorkbenchFailureCode::Protocol
        );
        peer.join().unwrap();
    }

    #[test]
    fn native_gateway_accepts_fragmented_responses() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut version = [0_u8; 4];
            stream.read_exact(&mut version).unwrap();
            let _client = read_string(&mut stream);
            let _content_type = read_string(&mut stream);
            let _payload = read_string(&mut stream);
            let response = [
                &("Ok".len() as i32).to_le_bytes()[..],
                b"Ok",
                &(r#"{"IsRunning":true,"ScriptsCompiled":true}"#.len() as i32).to_le_bytes()[..],
                br#"{"IsRunning":true,"ScriptsCompiled":true}"#,
            ]
            .concat();
            for byte in response {
                stream.write_all(&[byte]).unwrap();
                stream.flush().unwrap();
            }
        });
        let gateway = test_gateway(port);

        assert_eq!(
            gateway.status().unwrap(),
            WorkbenchStatus {
                is_running: true,
                scripts_compiled: true,
            }
        );
        peer.join().unwrap();
    }

    #[test]
    fn premature_native_close_is_a_protocol_failure() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut version = [0_u8; 4];
            stream.read_exact(&mut version).unwrap();
            let _client = read_string(&mut stream);
            let _content_type = read_string(&mut stream);
            let _payload = read_string(&mut stream);
            stream.write_all(&4_i32.to_le_bytes()).unwrap();
            stream.write_all(b"Ok").unwrap();
        });
        let gateway = test_gateway(port);

        assert_eq!(
            gateway.status().unwrap_err().code,
            WorkbenchFailureCode::Protocol
        );
        peer.join().unwrap();
    }

    #[test]
    fn native_workbench_error_is_distinct_from_protocol_failure() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut version = [0_u8; 4];
            stream.read_exact(&mut version).unwrap();
            let _client = read_string(&mut stream);
            let _content_type = read_string(&mut stream);
            let _payload = read_string(&mut stream);
            write_string(&mut stream, "WorkbenchError");
            write_string(&mut stream, "{}");
        });
        let gateway = test_gateway(port);

        assert_eq!(
            gateway.status().unwrap_err().code,
            WorkbenchFailureCode::WorkbenchError
        );
        peer.join().unwrap();
    }

    #[test]
    fn refused_native_endpoint_is_unavailable() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let gateway = test_gateway(port);

        assert_eq!(
            gateway.status().unwrap_err().code,
            WorkbenchFailureCode::Unavailable
        );
    }

    #[test]
    fn oversized_native_response_is_rejected_before_allocation() {
        use std::net::TcpListener;

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut version = [0_u8; 4];
            stream.read_exact(&mut version).unwrap();
            let _client = read_string(&mut stream);
            let _content_type = read_string(&mut stream);
            let _payload = read_string(&mut stream);
            write_string(&mut stream, "Ok");
            stream
                .write_all(&((super::MAX_RESPONSE_BYTES as i32) + 1).to_le_bytes())
                .unwrap();
        });
        let gateway = test_gateway(port);

        assert_eq!(
            gateway.status().unwrap_err().code,
            WorkbenchFailureCode::Protocol
        );
        peer.join().unwrap();
    }

    #[test]
    fn managed_installation_preserves_unknown_profile_scripts() {
        let root = std::env::temp_dir().join(format!(
            "rst-workbench-install-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let profile = root
            .join("Documents")
            .join("My Games")
            .join("ArmaReforgerWorkbench")
            .join("profile");
        let bridge = profile
            .join("scripts")
            .join("WorkbenchGame")
            .join("reforger-script-tools");
        fs::create_dir_all(&bridge).unwrap();
        fs::write(bridge.join("user-script.c"), "keep me").unwrap();
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            user_directory: Some(root.clone()),
            ..super::WorkbenchControllerOptions::default()
        });

        controller.write_managed_files(&bridge).unwrap();

        assert_eq!(
            fs::read_to_string(bridge.join("user-script.c")).unwrap(),
            "keep me"
        );
        assert!(bridge.join("reforger-script-tools.manifest.json").is_file());
        let status = controller.bridge_disk_status(&bridge);
        assert!(status.installed);
        assert!(!status.maintenance_required);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn migrates_the_known_flat_profile_package_into_workbench_game() {
        let root = test_root("migrate-flat-profile-package");
        let scripts = root.join("scripts");
        let legacy = scripts.join("reforger-script-tools");
        let destination = scripts.join("WorkbenchGame").join("reforger-script-tools");
        let controller =
            super::WorkbenchController::new(super::WorkbenchControllerOptions::default());
        controller.write_managed_files(&legacy).unwrap();
        fs::write(legacy.join("user-script.c"), "preserve me").unwrap();

        assert!(controller
            .migrate_legacy_bridge(&legacy, &destination)
            .unwrap());
        assert!(destination
            .join("reforger-script-tools.manifest.json")
            .is_file());
        for (name, _) in super::bridge_payload() {
            assert!(destination.join(name).is_file());
            assert!(!legacy.join(name).exists());
        }
        assert_eq!(
            fs::read_to_string(legacy.join("user-script.c")).unwrap(),
            "preserve me"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn first_install_validates_scripts_without_probing_the_new_handler() {
        let root = test_root("first-install-reload-required");
        let profile = root
            .join("Documents")
            .join("My Games")
            .join("ArmaReforgerWorkbench")
            .join("profile");
        fs::create_dir_all(&profile).unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = thread::spawn(move || {
            for (expected, response) in [
                (
                    json!({"APIFunc":"IsWorkbenchRunning"}),
                    json!({"IsRunning":true,"ScriptsCompiled":true}),
                ),
                (
                    json!({"APIFunc":"ValidateScripts","Configuration":"WORKBENCH"}),
                    json!({"Success":true,"Errors":[],"Warnings":[]}),
                ),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut version = [0_u8; 4];
                stream.read_exact(&mut version).unwrap();
                let _client = read_string(&mut stream);
                let _content_type = read_string(&mut stream);
                let request: Value = serde_json::from_str(&read_string(&mut stream)).unwrap();
                assert_eq!(request, expected);
                write_string(&mut stream, "Ok");
                write_string(&mut stream, &response.to_string());
            }
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                validation_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            user_directory: Some(root.clone()),
            ..super::WorkbenchControllerOptions::default()
        });

        let result = controller
            .install_bridge(super::WorkbenchInstallAuthorization::UserApprovedFirstInstall)
            .unwrap();

        assert_eq!(result.installed_version, super::WORKBENCH_BRIDGE_VERSION);
        assert!(!result.activated);
        assert_eq!(result.active_version, None);
        assert_eq!(result.protocol_version, None);
        assert_eq!(result.managed_files, super::bridge_payload().len());
        peer.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn existing_consent_install_does_not_create_a_first_installation() {
        let root = test_root("install-consent-required");
        let profile = root
            .join("Documents")
            .join("My Games")
            .join("ArmaReforgerWorkbench")
            .join("profile");
        let bridge = profile.join("scripts").join("reforger-script-tools");
        fs::create_dir_all(&profile).unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut version = [0_u8; 4];
            stream.read_exact(&mut version).unwrap();
            let _client = read_string(&mut stream);
            let _content_type = read_string(&mut stream);
            let request: Value = serde_json::from_str(&read_string(&mut stream)).unwrap();
            assert_eq!(request, json!({"APIFunc":"IsWorkbenchRunning"}));
            write_string(&mut stream, "Ok");
            write_string(
                &mut stream,
                &json!({"IsRunning":true,"ScriptsCompiled":true}).to_string(),
            );
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                validation_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            user_directory: Some(root.clone()),
            ..super::WorkbenchControllerOptions::default()
        });

        let failure = controller
            .install_bridge(super::WorkbenchInstallAuthorization::ExistingConsent)
            .unwrap_err();

        assert_eq!(failure.code, super::WorkbenchFailureCode::ConsentRequired);
        assert!(!bridge.exists());
        peer.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_status_probe_does_not_run_managed_maintenance() {
        let root = test_root("native-status-no-maintenance");
        let bridge = root
            .join("Documents")
            .join("My Games")
            .join("ArmaReforgerWorkbench")
            .join("profile")
            .join("scripts")
            .join("reforger-script-tools");
        fs::create_dir_all(&bridge).unwrap();
        fs::write(
            bridge.join("reforger-script-tools.manifest.json"),
            serde_json::to_vec_pretty(&super::BridgeManifest {
                bridge_version: super::WORKBENCH_BRIDGE_VERSION.to_string(),
                protocol_version: super::WORKBENCH_BRIDGE_PROTOCOL_VERSION,
                files: Vec::new(),
            })
            .unwrap(),
        )
        .unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut version = [0_u8; 4];
            stream.read_exact(&mut version).unwrap();
            let _client = read_string(&mut stream);
            let _content_type = read_string(&mut stream);
            let request: Value = serde_json::from_str(&read_string(&mut stream)).unwrap();
            assert_eq!(request, json!({"APIFunc":"IsWorkbenchRunning"}));
            write_string(&mut stream, "Ok");
            write_string(
                &mut stream,
                &json!({"IsRunning":true,"ScriptsCompiled":false}).to_string(),
            );
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                validation_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            user_directory: Some(root.clone()),
            ..super::WorkbenchControllerOptions::default()
        });

        controller.native_status().unwrap();

        assert!(
            !bridge.join("RST_WorkbenchCapabilities.c").exists(),
            "heartbeat status must not repair or retry the managed package"
        );
        peer.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn automatic_maintenance_never_downgrades_a_newer_managed_package() {
        let root = std::env::temp_dir().join(format!(
            "rst-workbench-newer-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let bridge = root.join("bridge");
        fs::create_dir_all(&bridge).unwrap();
        fs::write(bridge.join("future.c"), "future package").unwrap();
        fs::write(
            bridge.join("reforger-script-tools.manifest.json"),
            serde_json::to_vec_pretty(&super::BridgeManifest {
                bridge_version: "9.0.0".to_string(),
                protocol_version: 9,
                files: vec![super::BridgeManifestFile {
                    name: "future.c".to_string(),
                    sha256: super::sha256(b"future package"),
                }],
            })
            .unwrap(),
        )
        .unwrap();
        let controller =
            super::WorkbenchController::new(super::WorkbenchControllerOptions::default());

        controller.repair_managed_files(&bridge).unwrap();

        assert_eq!(
            fs::read_to_string(bridge.join("future.c")).unwrap(),
            "future package"
        );
        assert!(!bridge.join("RST_WorkbenchCapabilities.c").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_install_is_idempotent_and_does_not_downgrade_a_newer_package() {
        let root = test_root("install-newer");
        let profile = root
            .join("Documents")
            .join("My Games")
            .join("ArmaReforgerWorkbench")
            .join("profile");
        let bridge = profile
            .join("scripts")
            .join("WorkbenchGame")
            .join("reforger-script-tools");
        fs::create_dir_all(&bridge).unwrap();
        fs::write(bridge.join("future.c"), "future package").unwrap();
        fs::write(
            bridge.join("reforger-script-tools.manifest.json"),
            serde_json::to_vec_pretty(&super::BridgeManifest {
                bridge_version: "9.0.0".to_string(),
                protocol_version: 9,
                files: vec![super::BridgeManifestFile {
                    name: "future.c".to_string(),
                    sha256: super::sha256(b"future package"),
                }],
            })
            .unwrap(),
        )
        .unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = thread::spawn(move || {
            for (expected, response) in [
                (
                    json!({"APIFunc":"IsWorkbenchRunning"}),
                    json!({"IsRunning":true,"ScriptsCompiled":true}),
                ),
                (
                    json!({"APIFunc":"RST_WorkbenchCapabilities"}),
                    json!({"bridgeVersion":"9.0.0","protocolVersion":9,"capabilities":"future"}),
                ),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut version = [0_u8; 4];
                stream.read_exact(&mut version).unwrap();
                let _client = read_string(&mut stream);
                let _content_type = read_string(&mut stream);
                let request: Value = serde_json::from_str(&read_string(&mut stream)).unwrap();
                assert_eq!(request, expected);
                write_string(&mut stream, "Ok");
                write_string(&mut stream, &response.to_string());
            }
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                validation_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            user_directory: Some(root.clone()),
            ..super::WorkbenchControllerOptions::default()
        });

        let result = controller
            .install_bridge(super::WorkbenchInstallAuthorization::ExistingConsent)
            .unwrap();

        assert_eq!(result.installed_version, "9.0.0");
        assert!(!result.activated);
        assert_eq!(
            fs::read_to_string(bridge.join("future.c")).unwrap(),
            "future package"
        );
        assert!(!bridge.join("RST_WorkbenchCapabilities.c").exists());
        peer.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn managed_version_order_uses_semver_and_preserves_unknown_installed_versions() {
        use std::cmp::Ordering;

        assert_eq!(
            super::version_order("1.0.1-beta.1", "1.0.0"),
            Ordering::Greater
        );
        assert_eq!(
            super::version_order("1.0.0-beta.1", "1.0.0"),
            Ordering::Less
        );
        assert_eq!(super::version_order("1.0.0", "1.0.0"), Ordering::Equal);
        assert_eq!(
            super::version_order("future-channel", "1.0.0"),
            Ordering::Greater
        );
    }

    #[test]
    fn integration_log_rotates_and_emits_bounded_support_records() {
        let root = test_root("integration-log");
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            user_directory: Some(root.clone()),
            ..super::WorkbenchControllerOptions::default()
        });
        let path = controller.integration_log_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, vec![b'x'; 1024 * 1024 + 1]).unwrap();

        let reference = controller.log_event_timed(
            "test-operation",
            "test-outcome",
            std::time::Instant::now(),
            json!({"phase":"fixture"}),
        );

        assert!(path.with_extension("log.1").is_file());
        let record: Value =
            serde_json::from_str(fs::read_to_string(&path).unwrap().trim()).unwrap();
        assert_eq!(record.get("reference"), Some(&json!(reference)));
        assert_eq!(record.get("operation"), Some(&json!("test-operation")));
        assert_eq!(record.get("outcome"), Some(&json!("test-outcome")));
        assert_eq!(record.pointer("/details/phase"), Some(&json!("fixture")));
        assert!(record.get("sourceText").is_none());
        assert!(record.get("netApiPayload").is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mutation_audit_records_identify_the_action_without_recording_values() {
        let entity_result = super::WorkbenchEntityMutationResult {
            bridge_version: "1.51.0".to_string(),
            protocol_version: 1,
            status: "available".to_string(),
            active_layer_id: Some(3),
            entity: None,
            confirmation_token: None,
            destination: None,
            destination_exists: None,
            resource_name: None,
            persistence_path: None,
            template_saved: None,
            inspection: None,
        };
        let entity_details = super::entity_mutation_audit_details(
            &json!({
                "entityId": "0x10",
                "propertyName": "m_iNumTest",
                "value": "must-not-be-logged",
            }),
            &entity_result,
        );

        assert_eq!(
            super::entity_mutation_operation("RST_WorkbenchSetEntityProperty"),
            "set-entity-property"
        );
        assert_eq!(
            super::entity_mutation_operation("RST_WorkbenchCreatePrefab"),
            "create-prefab"
        );
        assert_eq!(
            super::entity_mutation_operation("RST_WorkbenchSavePrefab"),
            "save-prefab"
        );
        assert_eq!(
            super::entity_mutation_operation("RST_WorkbenchSetPrefabProperty"),
            "set-prefab-property"
        );
        assert_eq!(entity_details.get("entityId"), Some(&json!("0x10")));
        assert_eq!(
            entity_details.get("propertyName"),
            Some(&json!("m_iNumTest"))
        );
        assert!(entity_details.get("value").is_none());

        let component_details = super::component_mutation_audit_details(
            &json!({
                "entityId": "0x10",
                "className": "GRAY_TEST",
                "propertyName": "m_iNumTest",
                "value": 40,
            }),
            &super::WorkbenchComponentResult {
                bridge_version: "1.51.0".to_string(),
                protocol_version: 1,
                status: "available".to_string(),
                entity: None,
                components: Vec::new(),
                properties: Vec::new(),
                confirmation_token: None,
            },
        );
        assert_eq!(
            super::component_mutation_operation("RST_WorkbenchSetComponentProperty"),
            Some("set-component-property")
        );
        assert_eq!(
            super::component_mutation_operation("RST_WorkbenchSetPrefabComponentProperty"),
            Some("set-prefab-component-property")
        );
        assert_eq!(
            component_details.get("componentClass"),
            Some(&json!("GRAY_TEST"))
        );
        assert!(component_details.get("value").is_none());
    }

    #[test]
    fn known_log_tail_is_bounded_and_reports_truncation() {
        let root = test_root("log-tail");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("console.log");
        fs::write(
            &path,
            (1..=600)
                .map(|line| format!("line {line}\n"))
                .collect::<String>(),
        )
        .unwrap();

        let (lines, truncated) = super::bounded_log_tail(&path, 500).unwrap();

        assert!(truncated);
        assert_eq!(lines.len(), 500);
        assert_eq!(lines.first().map(String::as_str), Some("line 101"));
        assert_eq!(lines.last().map(String::as_str), Some("line 600"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn restart_project_resolution_requires_one_exact_gproj_title() {
        let root = test_root("restart-project");
        let addons = root.join("addons");
        let matching = addons.join("Test Bullshit").join("addon.gproj");
        fs::create_dir_all(matching.parent().unwrap()).unwrap();
        fs::write(&matching, "ID \"TestBullshit\"\nTITLE \"Test Bullshit\"\n").unwrap();

        assert_eq!(
            super::resolve_project_gproj(&root, "Test Bullshit"),
            Some(matching.clone())
        );
        fs::write(
            addons.join("duplicate.gproj"),
            "ID \"Duplicate\"\nTITLE \"Test Bullshit\"\n",
        )
        .unwrap();
        assert_eq!(super::resolve_project_gproj(&root, "Test Bullshit"), None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn base_game_addon_directory_requires_the_reforger_project_descriptor() {
        let root = test_root("base-game-addons");
        let addons = root.join("addons");
        fs::create_dir_all(addons.join("data")).unwrap();

        assert_eq!(super::base_game_addons_directory(Some(&root)), None);

        fs::write(
            addons.join("data").join("ArmaReforger.gproj"),
            "GameProject {}",
        )
        .unwrap();
        assert_eq!(super::base_game_addons_directory(Some(&root)), Some(addons));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workbench_launch_arguments_load_the_required_base_addons() {
        let root = test_root("launch-arguments");
        let game = root.join("game");
        let project = root.join("project").join("Example.gproj");
        fs::create_dir_all(game.join("addons").join("data")).unwrap();
        fs::create_dir_all(project.parent().unwrap()).unwrap();
        fs::write(
            game.join("addons").join("data").join("ArmaReforger.gproj"),
            "{}",
        )
        .unwrap();

        assert_eq!(
            super::workbench_launch_arguments(
                &super::WorkbenchHost::Native,
                None,
                Some(&game),
                None,
            ),
            Some(vec![
                std::ffi::OsString::from("-noThrow"),
                std::ffi::OsString::from("-forceUpdate"),
                std::ffi::OsString::from("-addons"),
                std::ffi::OsString::from("58D0FB3206B6F859,5614BBCCBB55ED1C"),
                std::ffi::OsString::from("-addonsDir"),
                game.join("addons").into_os_string(),
            ]),
        );
        assert_eq!(
            super::workbench_launch_arguments(&super::WorkbenchHost::Native, None, None, None),
            None,
        );
        assert_eq!(
            super::workbench_launch_arguments(
                &super::WorkbenchHost::Native,
                Some(&project),
                Some(&game),
                None,
            ),
            Some(vec![
                std::ffi::OsString::from("-noThrow"),
                std::ffi::OsString::from("-forceUpdate"),
                std::ffi::OsString::from("-gproj"),
                project.clone().into_os_string(),
                std::ffi::OsString::from("-addons"),
                std::ffi::OsString::from("58D0FB3206B6F859,5614BBCCBB55ED1C"),
                std::ffi::OsString::from("-addonsDir"),
                game.join("addons").into_os_string(),
            ]),
        );
        let profile_root = root.join("isolated-workbench");
        assert_eq!(
            super::workbench_launch_arguments(
                &super::WorkbenchHost::Native,
                Some(&project),
                Some(&game),
                Some(&profile_root),
            ),
            Some(vec![
                std::ffi::OsString::from("-noThrow"),
                std::ffi::OsString::from("-forceUpdate"),
                std::ffi::OsString::from("-profile"),
                profile_root.into_os_string(),
                std::ffi::OsString::from("-gproj"),
                project.into_os_string(),
                std::ffi::OsString::from("-addons"),
                std::ffi::OsString::from("58D0FB3206B6F859,5614BBCCBB55ED1C"),
                std::ffi::OsString::from("-addonsDir"),
                game.join("addons").into_os_string(),
            ]),
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn automatic_maintenance_repairs_missing_modified_old_and_inconsistent_files() {
        let root = test_root("managed-repair");
        let bridge = root.join("bridge");
        let controller =
            super::WorkbenchController::new(super::WorkbenchControllerOptions::default());
        controller.write_managed_files(&bridge).unwrap();
        fs::write(
            bridge.join("RST_WorkbenchCapabilities.c"),
            "modified managed file",
        )
        .unwrap();
        fs::remove_file(bridge.join("RST_WorkbenchState.c")).unwrap();

        assert!(controller.repair_managed_files(&bridge).unwrap());
        for (name, content) in super::bridge_payload() {
            assert_eq!(fs::read(bridge.join(name)).unwrap(), content.as_bytes());
        }

        let manifest_path = bridge.join("reforger-script-tools.manifest.json");
        let mut tampered: super::BridgeManifest =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        tampered.files[0].sha256 = "incorrect-hash".to_string();
        tampered.files.push(super::BridgeManifestFile {
            name: "RST_ObsoleteCurrent.c".to_string(),
            sha256: super::sha256(b"obsolete current"),
        });
        fs::write(bridge.join("RST_ObsoleteCurrent.c"), "obsolete current").unwrap();
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&tampered).unwrap(),
        )
        .unwrap();

        assert!(controller.repair_managed_files(&bridge).unwrap());
        assert!(!bridge.join("RST_ObsoleteCurrent.c").exists());
        let corrected: super::BridgeManifest =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        assert!(super::manifest_matches_payload(&corrected));

        let mut manifest: super::BridgeManifest =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        manifest.bridge_version = "0.9.0".to_string();
        manifest.protocol_version = 9;
        manifest.files.push(super::BridgeManifestFile {
            name: "RST_Obsolete.c".to_string(),
            sha256: super::sha256(b"obsolete"),
        });
        fs::write(bridge.join("RST_Obsolete.c"), "obsolete").unwrap();
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        assert!(controller.repair_managed_files(&bridge).unwrap());
        assert!(!bridge.join("RST_Obsolete.c").exists());
        let repaired: super::BridgeManifest =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        assert_eq!(repaired.bridge_version, super::WORKBENCH_BRIDGE_VERSION);
        assert_eq!(
            repaired.protocol_version,
            super::WORKBENCH_BRIDGE_PROTOCOL_VERSION
        );

        repaired.files.iter().for_each(|file| {
            assert_eq!(
                file.sha256,
                super::sha256(&fs::read(bridge.join(&file.name)).unwrap())
            )
        });

        let mut inconsistent = repaired;
        inconsistent.protocol_version = 9;
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&inconsistent).unwrap(),
        )
        .unwrap();
        assert!(controller.repair_managed_files(&bridge).unwrap());
        let repaired_protocol: super::BridgeManifest =
            serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
        assert_eq!(
            repaired_protocol.protocol_version,
            super::WORKBENCH_BRIDGE_PROTOCOL_VERSION
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn maintenance_does_not_probe_an_unregistered_handler() {
        let root = test_root("activation-retry");
        let bridge = root.join("bridge");
        let controller =
            super::WorkbenchController::new(super::WorkbenchControllerOptions::default());
        controller.write_managed_files(&bridge).unwrap();

        let status = controller.maintain_existing_bridge(&bridge);

        assert_eq!(status.active_version, None);
        assert_eq!(status.protocol_version, None);
        assert!(!status.compatible);
        assert!(status.activation_required);
        assert!(status.capabilities.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn incompatible_handler_protocol_exposes_no_dependent_capabilities() {
        let root = test_root("incompatible-handler");
        let bridge = root.join("bridge");
        let (port, peer) = start_peer(|request| {
            assert_eq!(request, json!({"APIFunc":"RST_WorkbenchCapabilities"}));
            json!({
                "bridgeVersion":"1.0.0",
                "protocolVersion":9,
                "capabilities":"state;reload"
            })
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                validation_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            ..super::WorkbenchControllerOptions::default()
        });
        controller.write_managed_files(&bridge).unwrap();

        let status = controller.active_bridge_status(&bridge, false);

        assert!(!status.compatible);
        assert!(status.activation_required);
        assert!(status.capabilities.is_empty());
        assert!(!status.capabilities_truncated);
        peer.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn obsolete_manifest_entries_cannot_escape_the_managed_directory() {
        let root = std::env::temp_dir().join(format!(
            "rst-workbench-containment-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let bridge = root.join("bridge");
        fs::create_dir_all(&bridge).unwrap();
        fs::write(root.join("outside.c"), "keep").unwrap();
        fs::write(bridge.join("obsolete.c"), "remove").unwrap();
        fs::write(
            bridge.join("reforger-script-tools.manifest.json"),
            serde_json::to_vec_pretty(&super::BridgeManifest {
                bridge_version: "0.9.0".to_string(),
                protocol_version: 1,
                files: vec![
                    super::BridgeManifestFile {
                        name: "obsolete.c".to_string(),
                        sha256: super::sha256(b"remove"),
                    },
                    super::BridgeManifestFile {
                        name: "../outside.c".to_string(),
                        sha256: super::sha256(b"keep"),
                    },
                ],
            })
            .unwrap(),
        )
        .unwrap();
        let controller =
            super::WorkbenchController::new(super::WorkbenchControllerOptions::default());

        controller.write_managed_files(&bridge).unwrap();

        assert!(!bridge.join("obsolete.c").exists());
        assert_eq!(fs::read_to_string(root.join("outside.c")).unwrap(), "keep");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_workbench_paths_override_discovery_and_keep_the_profile_user_relative() {
        let root = test_root("explicit-paths");
        let game = root.join("game");
        let tools = root.join("tools");
        let executable = tools
            .join("Workbench")
            .join("ArmaReforgerWorkbenchSteamDiag.exe");
        fs::create_dir_all(&game).unwrap();
        fs::create_dir_all(executable.parent().unwrap()).unwrap();
        fs::write(&executable, b"fixture").unwrap();
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            user_directory: Some(root.clone()),
            game_directory: Some(game.clone()),
            tools_directory: Some(tools.clone()),
            executable: Some(executable.clone()),
            ..super::WorkbenchControllerOptions::default()
        });

        let paths = controller.paths();

        assert_eq!(paths.game, Some(game));
        assert_eq!(paths.game_source, "explicit");
        assert_eq!(paths.tools, Some(tools));
        assert_eq!(paths.tools_source, "explicit");
        assert_eq!(paths.executable, Some(executable));
        assert_eq!(paths.executable_source, "explicit");
        assert_eq!(
            paths.profile,
            root.join("Documents")
                .join("My Games")
                .join("ArmaReforgerWorkbench")
                .join("profile")
        );
        assert_eq!(
            paths.bridge_directory,
            paths
                .profile
                .join("scripts")
                .join("WorkbenchGame")
                .join("reforger-script-tools")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn steam_discovery_resolves_game_and_tools_from_independent_libraries() {
        let root = test_root("steam-independent-libraries");
        let steam = root.join("Steam");
        let game_library = root.join("GameLibrary");
        let tools_library = root.join("ToolsLibrary");
        write_library_folders(&steam, &[&game_library, &tools_library]);
        write_steam_app(&game_library, "1874880", "Arma Reforger", "Arma Reforger");
        write_steam_app(
            &tools_library,
            "1874910",
            "Arma Reforger Tools",
            "Arma Reforger Tools",
        );

        assert_eq!(
            super::discover_steam_app_from_root(&steam, "1874880"),
            super::SteamAppDiscovery::Found(
                game_library
                    .join("steamapps")
                    .join("common")
                    .join("Arma Reforger")
            )
        );
        assert_eq!(
            super::discover_steam_app_from_root(&steam, "1874910"),
            super::SteamAppDiscovery::Found(
                tools_library
                    .join("steamapps")
                    .join("common")
                    .join("Arma Reforger Tools")
            )
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn steam_discovery_requires_manifest_metadata_and_rejects_ambiguity() {
        let root = test_root("steam-ambiguity");
        let steam = root.join("Steam");
        assert_eq!(
            super::discover_steam_app_from_roots(&[], "1874910"),
            super::SteamAppDiscovery::RegistrationUnavailable
        );
        let canonical = steam
            .join("steamapps")
            .join("common")
            .join("Arma Reforger Tools");
        fs::create_dir_all(&canonical).unwrap();

        assert_eq!(
            super::discover_steam_app_from_root(&steam, "1874910"),
            super::SteamAppDiscovery::ManifestUnavailable
        );

        let invalid = root.join("InvalidLibrary");
        write_steam_app(
            &invalid,
            "1874910",
            "Arma Reforger Tools",
            "Arma Reforger Tools",
        );
        fs::remove_file(
            invalid
                .join("steamapps")
                .join("common")
                .join("Arma Reforger Tools")
                .join("Workbench")
                .join("ArmaReforgerWorkbenchSteamDiag.exe"),
        )
        .unwrap();
        assert_eq!(
            super::discover_steam_app_from_root(&invalid, "1874910"),
            super::SteamAppDiscovery::InvalidInstallation
        );

        let other = root.join("OtherLibrary");
        write_library_folders(&steam, &[&other]);
        write_steam_app(
            &other,
            "1874910",
            "Arma Reforger Tools",
            "Arma Reforger Tools",
        );
        assert_eq!(
            super::discover_steam_app_from_root(&steam, "1874910"),
            super::SteamAppDiscovery::Found(
                other
                    .join("steamapps")
                    .join("common")
                    .join("Arma Reforger Tools")
            )
        );

        let second = root.join("SecondLibrary");
        write_library_folders(&steam, &[&other, &second]);
        write_steam_app(
            &second,
            "1874910",
            "Arma Reforger Tools",
            "Arma Reforger Tools",
        );
        assert_eq!(
            super::discover_steam_app_from_root(&steam, "1874910"),
            super::SteamAppDiscovery::AmbiguousInstallations
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workbench_executable_shape_is_exact() {
        let root = test_root("executable-shape");
        let directory = root.join("Tools").join("Workbench");
        fs::create_dir_all(&directory).unwrap();
        let expected = directory.join("ArmaReforgerWorkbenchSteamDiag.exe");
        let wrong_name = directory.join("ArmaReforgerWorkbench.exe");
        fs::write(&expected, b"fixture").unwrap();
        fs::write(&wrong_name, b"fixture").unwrap();

        assert!(super::is_workbench_executable(&expected));
        assert!(!super::is_workbench_executable(&wrong_name));
        assert!(!super::is_workbench_executable(
            &directory
                .join("missing")
                .join("ArmaReforgerWorkbenchSteamDiag.exe")
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn validation_cursor_pages_one_immutable_compiler_snapshot() {
        let controller =
            super::WorkbenchController::new(super::WorkbenchControllerOptions::default());
        let diagnostics = (1..=3)
            .map(|line| super::WorkbenchCompilerDiagnostic {
                severity: super::WorkbenchDiagnosticSeverity::Error,
                message: format!("finding {line}"),
                location: super::WorkbenchDiagnosticLocation {
                    file: "scripts/Test.c".to_string(),
                    file_abs: None,
                    addon: Some("Test".to_string()),
                    line,
                },
            })
            .collect();
        *controller.validation_snapshot.lock().unwrap() = Some((
            "snapshot".to_string(),
            super::WorkbenchValidation {
                profile: "WORKBENCH".to_string(),
                success: false,
                diagnostics,
            },
        ));

        let page = controller
            .validate_scripts_page(Some("wv1:snapshot:1"), 1)
            .unwrap();

        assert_eq!(page.total_diagnostics, 3);
        assert_eq!(page.diagnostics[0].message, "finding 2");
        assert_eq!(page.next_cursor.as_deref(), Some("wv1:snapshot:2"));
    }

    fn test_root(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "rst-workbench-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn write_library_folders(steam: &std::path::Path, libraries: &[&std::path::Path]) {
        let steamapps = steam.join("steamapps");
        fs::create_dir_all(&steamapps).unwrap();
        let entries = libraries
            .iter()
            .enumerate()
            .map(|(index, library)| {
                format!(
                    "\"{}\"\n{{\n\"path\"\t\"{}\"\n}}\n",
                    index + 1,
                    library.display()
                )
            })
            .collect::<String>();
        fs::write(
            steamapps.join("libraryfolders.vdf"),
            format!("\"libraryfolders\"\n{{\n{entries}}}\n"),
        )
        .unwrap();
    }

    fn write_steam_app(library: &std::path::Path, app_id: &str, install_dir: &str, folder: &str) {
        let steamapps = library.join("steamapps");
        let installation = steamapps.join("common").join(folder);
        fs::create_dir_all(&installation).unwrap();
        match app_id {
            "1874880" => {
                let project = installation
                    .join("addons")
                    .join("data")
                    .join("ArmaReforger.gproj");
                fs::create_dir_all(project.parent().unwrap()).unwrap();
                fs::write(project, "GameProject {}").unwrap();
            }
            "1874910" => {
                let executable = installation
                    .join("Workbench")
                    .join("ArmaReforgerWorkbenchSteamDiag.exe");
                fs::create_dir_all(executable.parent().unwrap()).unwrap();
                fs::write(executable, b"fixture").unwrap();
            }
            _ => {}
        }
        fs::write(
            steamapps.join(format!("appmanifest_{app_id}.acf")),
            format!("\"AppState\"\n{{\n\"installdir\"\t\"{install_dir}\"\n}}\n"),
        )
        .unwrap();
    }

    #[test]
    fn typed_property_values_are_normalized_and_reject_mismatched_wire_values() {
        assert_eq!(super::normalized_property_value("float", "2.5"), json!(2.5));
        assert_eq!(
            super::normalized_property_value("vector", "1 2 3"),
            json!({"x": 1.0, "y": 2.0, "z": 3.0})
        );
        assert_eq!(
            super::property_value_wire_format("bool", &json!(true)).as_deref(),
            Some("1")
        );
        assert_eq!(
            super::property_value_wire_format("float", &json!("not-a-number")),
            None
        );
        assert_eq!(
            super::property_value_wire_format("vector", &json!({"x": 1.0, "y": 2.0})),
            None
        );
    }

    #[test]
    fn shape_point_wire_records_require_three_finite_coordinates() {
        assert_eq!(
            super::parse_shape_points("1|2|3;-4.5|0|7"),
            Some(vec![
                super::WorkbenchEntityPosition {
                    x: 1.0,
                    y: 2.0,
                    z: 3.0,
                },
                super::WorkbenchEntityPosition {
                    x: -4.5,
                    y: 0.0,
                    z: 7.0,
                },
            ])
        );
        assert_eq!(super::parse_shape_points("1|2"), None);
        assert_eq!(super::parse_shape_points("1|2|NaN"), None);
    }

    #[test]
    fn shape_point_edit_uses_the_typed_native_handler() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(
                request,
                json!({
                    "APIFunc": "RST_WorkbenchEditShapePoints",
                    "entityId": "0x01 {}",
                    "operation": "insert",
                    "index": 1,
                    "count": 1,
                    "points": "1,2,3;4,5,6",
                })
            );
            json!({"bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,"protocolVersion":1,"status":"points-updated","entity":"0x01 {}|PolylineShapeEntity|0|1|10|20|30||||","shapeClass":"PolylineShapeEntity","closed":false,"points":"1|2|3;4|5|6"})
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            ..super::WorkbenchControllerOptions::default()
        });
        let result = controller
            .edit_shape_points(
                "0x01 {}",
                super::WorkbenchShapePointEdit::Insert,
                Some(1),
                None,
                &[
                    super::WorkbenchEntityPosition {
                        x: 1.0,
                        y: 2.0,
                        z: 3.0,
                    },
                    super::WorkbenchEntityPosition {
                        x: 4.0,
                        y: 5.0,
                        z: 6.0,
                    },
                ],
            )
            .unwrap();
        assert_eq!(result.status, "points-updated");
        assert_eq!(result.points.len(), 2);
        assert_eq!(result.shape_class.as_deref(), Some("PolylineShapeEntity"));
        peer.join().unwrap();
    }

    #[test]
    fn shape_geometry_conversion_uses_explicit_spaces_and_typed_handler() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(
                request,
                json!({
                    "APIFunc":"RST_WorkbenchShapeGeometry", "entityId":"0x01 {}", "operation":"convert",
                    "fromSpace":"local", "toSpace":"world", "points":"1,2,3"
                })
            );
            json!({"bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,"protocolVersion":1,"status":"converted","entity":"0x01 {}|PolylineShapeEntity|0|1|10|20|30||||","shapeClass":"PolylineShapeEntity","fromSpace":"local","toSpace":"world","points":"11|22|33"})
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            ..super::WorkbenchControllerOptions::default()
        });
        let result = controller
            .convert_shape_points(
                "0x01 {}",
                super::WorkbenchShapePointSpace::Local,
                super::WorkbenchShapePointSpace::World,
                &[super::WorkbenchEntityPosition {
                    x: 1.0,
                    y: 2.0,
                    z: 3.0,
                }],
            )
            .unwrap();
        assert_eq!(result.status, "converted");
        assert_eq!(result.points[0].x, 11.0);
        assert_eq!(result.from_space, "local");
        peer.join().unwrap();
    }

    #[test]
    fn shape_geometry_transform_uses_one_typed_operation_and_returns_points() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(
                request,
                json!({
                    "APIFunc":"RST_WorkbenchShapeGeometry", "entityId":"0x01 {}", "operation":"transform", "space":"world",
                    "transformOperation":"rotateXZ", "offsetX":0.0, "offsetY":0.0, "offsetZ":0.0,
                    "pivotX":10.0, "pivotY":0.0, "pivotZ":20.0, "degrees":90.0,
                    "scaleX":1.0, "scaleY":1.0, "scaleZ":1.0, "mirrorAxis":""
                })
            );
            json!({"bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,"protocolVersion":1,"status":"points-transformed","entity":"0x01 {}|SplineShapeEntity|0|1|10|20|30||||","shapeClass":"SplineShapeEntity","closed":false,"points":"10|0|21"})
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            ..super::WorkbenchControllerOptions::default()
        });
        let result = controller
            .transform_shape_points(
                "0x01 {}",
                super::WorkbenchShapePointSpace::World,
                super::WorkbenchShapeTransformOperation::RotateXz,
                super::WorkbenchEntityPosition {
                    x: 0.0,
                    y: 0.0,
                    z: 0.0,
                },
                super::WorkbenchEntityPosition {
                    x: 10.0,
                    y: 0.0,
                    z: 20.0,
                },
                90.0,
                super::WorkbenchEntityPosition {
                    x: 1.0,
                    y: 1.0,
                    z: 1.0,
                },
                "",
            )
            .unwrap();
        assert_eq!(result.status, "points-transformed");
        assert_eq!(result.shape_class.as_deref(), Some("SplineShapeEntity"));
        assert_eq!(
            result.points,
            vec![super::WorkbenchEntityPosition {
                x: 10.0,
                y: 0.0,
                z: 21.0
            }]
        );
        peer.join().unwrap();
    }

    #[test]
    fn shape_geometry_resample_uses_explicit_space_and_returns_metrics() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(
                request,
                json!({
                    "APIFunc":"RST_WorkbenchShapeGeometry", "entityId":"0x01 {}", "operation":"resample", "space":"local", "spacingMeters":2.5
                })
            );
            json!({"bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,"protocolVersion":1,"status":"polyline-resampled","entity":"0x01 {}|PolylineShapeEntity|0|1|10|20|30||||","shapeClass":"PolylineShapeEntity","closed":false,"points":"0|0|0;2.5|0|0;5|0|0","spacingMeters":2.5,"originalPointCount":2,"resultPointCount":3,"pathLength":5.0,"skippedZeroLengthSegments":0})
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            ..super::WorkbenchControllerOptions::default()
        });
        let result = controller
            .resample_polyline("0x01 {}", super::WorkbenchShapePointSpace::Local, 2.5)
            .unwrap();
        assert_eq!(result.status, "polyline-resampled");
        assert_eq!(result.original_point_count, 2);
        assert_eq!(result.result_point_count, 3);
        assert_eq!(result.path_length, 5.0);
        assert_eq!(result.points.last().unwrap().x, 5.0);
        peer.join().unwrap();
    }

    #[test]
    fn shape_geometry_bridge_uses_parent_aware_conversion_and_one_action_mutations() {
        assert!(super::BRIDGE_SHAPE_GEOMETRY_SOURCE.contains("shape.CoordToParent(values[i])"));
        assert!(super::BRIDGE_SHAPE_GEOMETRY_SOURCE.contains("shape.CoordToLocal(values[i])"));
        assert!(super::BRIDGE_SHAPE_GEOMETRY_SOURCE
            .contains("Reforger Script Tools: transform shape points"));
        assert!(super::BRIDGE_SHAPE_GEOMETRY_SOURCE
            .contains("Reforger Script Tools: resample polyline"));
        assert!(super::BRIDGE_SHAPE_GEOMETRY_SOURCE.contains("shape.GetPointCount();"));
        assert!(super::BRIDGE_SHAPE_GEOMETRY_SOURCE.contains("Encode(sampled, response.points)"));
        assert!(super::BRIDGE_SHAPE_GEOMETRY_SOURCE
            .contains("source.GetClassName() != \"PolylineShapeEntity\""));
        assert!(super::BRIDGE_SHAPE_GEOMETRY_SOURCE
            .contains("source.GetClassName() != \"SplineShapeEntity\""));
    }

    #[test]
    fn spline_bridge_preserves_native_tangent_modes_and_one_action_edits() {
        assert!(super::BRIDGE_SPLINE_SOURCE.contains("HasPointExplicitTangents"));
        assert!(super::BRIDGE_SPLINE_SOURCE.contains("GetTangents"));
        assert!(super::BRIDGE_SPLINE_SOURCE.contains("GenerateTesselatedShape"));
        assert!(super::BRIDGE_SPLINE_SOURCE.contains("SplinePointData"));
        assert!(super::BRIDGE_SPLINE_SOURCE.contains("ClearPointData"));
        assert!(super::BRIDGE_SPLINE_SOURCE.contains("Reforger Script Tools: edit spline"));
    }

    #[test]
    fn spline_inspection_reads_anchor_tangent_modes_and_handles() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(
                request,
                json!({
                    "APIFunc": "RST_WorkbenchSpline",
                    "entityId": "0x01 {}",
                    "operation": "inspect",
                    "space": "local",
                })
            );
            json!({
                "bridgeVersion":super::WORKBENCH_BRIDGE_VERSION,
                "protocolVersion":1,
                "status":"available",
                "entity":"0x01 {}|SplineShapeEntity|0|1|10|20|30||||",
                "shapeClass":"SplineShapeEntity",
                "closed":false,
                "anchors":"0,auto,0,0,0,0,0,0,0,0,0;1,explicit,10,0,0,-2,0,0,4,1,2",
                "samples":"",
                "sampleSpace":"local",
                "pathLength":10.0,
                "sampleCount":0,
            })
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            ..super::WorkbenchControllerOptions::default()
        });
        let result = controller
            .inspect_spline("0x01 {}", super::WorkbenchShapePointSpace::Local)
            .unwrap();
        assert_eq!(result.status, "available");
        assert_eq!(result.anchors.len(), 2);
        assert_eq!(
            result.anchors[1].tangent_mode,
            super::WorkbenchSplineTangentMode::Explicit
        );
        assert_eq!(result.anchors[1].out_tangent.x, 4.0);
        peer.join().unwrap();
    }

    #[test]
    fn spline_edit_replaces_anchors_and_closed_state_in_one_typed_request() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(
                request,
                json!({
                    "APIFunc": "RST_WorkbenchSpline",
                    "entityId": "0x01 {}",
                    "operation": "edit",
                    "space": "world",
                    "anchors": "0,auto,1,2,3,0,0,0,0,0,0;1,explicit,4,5,6,-1,0,0,2,0,1",
                    "hasClosed": true,
                    "closed": true,
                })
            );
            json!({
                "bridgeVersion":super::WORKBENCH_BRIDGE_VERSION,
                "protocolVersion":1,
                "status":"spline-updated",
                "entity":"0x01 {}|SplineShapeEntity|0|1|10|20|30||||",
                "shapeClass":"SplineShapeEntity",
                "closed":true,
                "anchors":"0,auto,1,2,3,0,0,0,0,0,0;1,explicit,4,5,6,-1,0,0,2,0,1",
                "samples":"",
                "sampleSpace":"world",
                "pathLength":5.0,
                "sampleCount":0,
            })
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            ..super::WorkbenchControllerOptions::default()
        });
        let result = controller
            .edit_spline(
                "0x01 {}",
                super::WorkbenchShapePointSpace::World,
                &[
                    super::WorkbenchSplineAnchorInput {
                        position: super::WorkbenchEntityPosition {
                            x: 1.0,
                            y: 2.0,
                            z: 3.0,
                        },
                        tangent_mode: super::WorkbenchSplineTangentModeInput::Auto,
                        in_tangent: None,
                        out_tangent: None,
                    },
                    super::WorkbenchSplineAnchorInput {
                        position: super::WorkbenchEntityPosition {
                            x: 4.0,
                            y: 5.0,
                            z: 6.0,
                        },
                        tangent_mode: super::WorkbenchSplineTangentModeInput::Explicit,
                        in_tangent: Some(super::WorkbenchEntityPosition {
                            x: -1.0,
                            y: 0.0,
                            z: 0.0,
                        }),
                        out_tangent: Some(super::WorkbenchEntityPosition {
                            x: 2.0,
                            y: 0.0,
                            z: 1.0,
                        }),
                    },
                ],
                Some(true),
            )
            .unwrap();
        assert_eq!(result.status, "spline-updated");
        assert!(result.closed);
        assert_eq!(
            result.anchors[0].position,
            super::WorkbenchEntityPosition {
                x: 1.0,
                y: 2.0,
                z: 3.0
            }
        );
        peer.join().unwrap();
    }

    #[test]
    fn spline_sampling_returns_bounded_points_and_path_metrics() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(
                request,
                json!({
                    "APIFunc": "RST_WorkbenchSpline",
                    "entityId": "0x01 {}",
                    "operation": "sample",
                    "space": "world",
                    "maxSamples": 3,
                })
            );
            json!({
                "bridgeVersion":super::WORKBENCH_BRIDGE_VERSION,
                "protocolVersion":1,
                "status":"sampled",
                "entity":"0x01 {}|SplineShapeEntity|0|1|10|20|30||||",
                "shapeClass":"SplineShapeEntity",
                "closed":false,
                "anchors":"0,auto,0,0,0,0,0,0,0,0,0;1,auto,10,0,0,0,0,0,0,0,0",
                "samples":"0,0,0;5,0,0;10,0,0",
                "sampleSpace":"world",
                "pathLength":10.0,
                "sampleCount":3,
            })
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            ..super::WorkbenchControllerOptions::default()
        });
        let result = controller
            .sample_spline("0x01 {}", super::WorkbenchShapePointSpace::World, 3)
            .unwrap();
        assert_eq!(result.status, "sampled");
        assert_eq!(result.sample_count, 3);
        assert_eq!(result.path_length, 10.0);
        assert_eq!(result.samples.last().unwrap().x, 10.0);
        peer.join().unwrap();
    }

    #[test]
    fn reparent_handler_rejects_a_descendant_parent_before_starting_an_action() {
        assert!(super::BRIDGE_ENTITY_MUTATION_SOURCE
            .contains("bool IsAncestor(IEntitySource entity, IEntitySource candidateParent)",));
        let descendant_guard = super::BRIDGE_ENTITY_MUTATION_SOURCE
            .find("IsAncestor(entity, parent)")
            .unwrap();
        let reparent_guard = &super::BRIDGE_ENTITY_MUTATION_SOURCE[descendant_guard..];
        assert!(reparent_guard.contains("api.IsEntityLayerLockedHierarchy"));
    }

    #[test]
    fn component_addition_accepts_only_a_loaded_script_component_class() {
        assert!(
            super::BRIDGE_COMPONENTS_SOURCE.contains("r.className.ToType()")
                && super::BRIDGE_COMPONENTS_SOURCE.contains("IsInherited(ScriptComponent)")
        );
    }

    #[test]
    fn entity_property_mutation_records_the_post_action_entity_state() {
        assert!(super::BRIDGE_PROPERTIES_SOURCE
            .contains("class RST_WorkbenchSetEntityProperty : RST_WorkbenchEntityMutationBase"));
        let record = super::BRIDGE_PROPERTIES_SOURCE
            .find("Record(api, response, entity);")
            .unwrap();
        let property_set = super::BRIDGE_PROPERTIES_SOURCE
            .find("response.status = \"property-set\"")
            .unwrap();
        assert!(record < property_set);
    }

    #[test]
    fn entity_property_write_uses_only_the_inspection_descriptor_binding() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(
                request,
                json!({
                    "APIFunc": "RST_WorkbenchSetEntityProperty",
                    "entityId": "0x01 {}",
                    "propertyName": "m_fRadius",
                    "expectedValue": "2.5",
                    "value": "3.75",
                })
            );
            json!({"bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,"protocolVersion":1,"status":"property-set","activeLayerId":7,"entity":""})
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            ..super::WorkbenchControllerOptions::default()
        });
        controller
            .property_write_descriptors
            .lock()
            .unwrap()
            .insert(
                "prop2:test".to_string(),
                super::PropertyWriteDescriptor {
                    entity_id: "0x01 {}".to_string(),
                    component_id: None,
                    property_name: "m_fRadius".to_string(),
                    data_type: "float".to_string(),
                    observed_value: "2.5".to_string(),
                    issued: Instant::now(),
                },
            );
        let result = controller
            .set_entity_property("0x01 {}", "prop2:test", json!(3.75))
            .unwrap();
        assert_eq!(result.status, "property-set");
        peer.join().unwrap();
    }

    #[test]
    fn component_inspection_issues_a_component_bound_property_descriptor() {
        let (port, peer) = start_peer(|request| {
            assert_eq!(
                request,
                json!({"APIFunc":"RST_WorkbenchInspectComponent","entityId":"0x01 {}","componentId":"cmp1:0:TestComponent"})
            );
            json!({"bridgeVersion": super::WORKBENCH_BRIDGE_VERSION,"protocolVersion":1,"status":"available","entity":"","components":"0|TestComponent","properties":"m_fRadius|float|2.5|1"})
        });
        let controller = super::WorkbenchController::new(super::WorkbenchControllerOptions {
            gateway: super::WorkbenchGatewayOptions {
                port,
                status_deadline: Duration::from_secs(1),
                ..super::WorkbenchGatewayOptions::default()
            },
            ..super::WorkbenchControllerOptions::default()
        });
        let result = controller
            .inspect_component("0x01 {}", "cmp1:0:TestComponent")
            .unwrap();
        assert_eq!(result.components.len(), 1);
        assert_eq!(result.properties.len(), 1);
        let property = serde_json::to_value(&result.properties[0]).unwrap();
        assert!(property.get("directlySet").is_none());
        assert!(property.get("writable").is_none());
        assert!(result.properties[0]
            .write_descriptor
            .as_deref()
            .is_some_and(|value| value.starts_with("prop2:")));
        peer.join().unwrap();
    }

    #[test]
    fn component_operations_reject_an_unissued_component_descriptor_before_gateway_dispatch() {
        let controller =
            super::WorkbenchController::new(super::WorkbenchControllerOptions::default());

        let result = controller
            .inspect_component("0x01 {}", "forged-component")
            .unwrap();

        assert_eq!(result.status, "invalid-component-descriptor");
    }

    /// A Wine host with the drive mapping every prefix has: the prefix's own
    /// `C:` and the host root as `Z:`.
    fn wine_test_host() -> super::WorkbenchHost {
        super::WorkbenchHost::Wine(crate::host_platform::WinePrefix::from_drives(
            std::path::PathBuf::from("/prefix"),
            crate::host_platform::WinePrefixSource::SteamCompatibilityData,
            vec![
                ('c', std::path::PathBuf::from("/prefix/drive_c")),
                ('z', std::path::PathBuf::from("/")),
            ],
        ))
    }

    fn test_gateway(port: u16) -> WorkbenchGateway {
        WorkbenchGateway::new(WorkbenchGatewayOptions {
            port,
            status_deadline: Duration::from_secs(1),
            validation_deadline: Duration::from_secs(1),
            ..WorkbenchGatewayOptions::default()
        })
    }

    fn start_peer(
        response: impl FnOnce(Value) -> Value + Send + 'static,
    ) -> (u16, thread::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut version = [0_u8; 4];
            stream.read_exact(&mut version).unwrap();
            assert_eq!(i32::from_le_bytes(version), 1);
            assert_eq!(read_string(&mut stream), "ReforgerScriptTools");
            assert_eq!(read_string(&mut stream), "JsonRPC");
            let payload: Value = serde_json::from_str(&read_string(&mut stream)).unwrap();
            let payload = response(payload);
            write_string(&mut stream, "Ok");
            write_string(&mut stream, &payload.to_string());
        });
        (port, peer)
    }

    fn start_peer_sequence(responses: Vec<(Value, Value)>) -> (u16, thread::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let peer = thread::spawn(move || {
            for (expected_request, response) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut version = [0_u8; 4];
                stream.read_exact(&mut version).unwrap();
                assert_eq!(i32::from_le_bytes(version), 1);
                assert_eq!(read_string(&mut stream), "ReforgerScriptTools");
                assert_eq!(read_string(&mut stream), "JsonRPC");
                assert_eq!(
                    serde_json::from_str::<Value>(&read_string(&mut stream)).unwrap(),
                    expected_request
                );
                write_string(&mut stream, "Ok");
                write_string(&mut stream, &response.to_string());
            }
        });
        (port, peer)
    }

    fn read_string(stream: &mut impl Read) -> String {
        let mut length = [0_u8; 4];
        stream.read_exact(&mut length).unwrap();
        let mut bytes = vec![0; i32::from_le_bytes(length) as usize];
        stream.read_exact(&mut bytes).unwrap();
        String::from_utf8(bytes).unwrap()
    }

    fn write_string(stream: &mut impl Write, value: &str) {
        stream
            .write_all(&(value.len() as i32).to_le_bytes())
            .unwrap();
        stream.write_all(value.as_bytes()).unwrap();
    }
}
