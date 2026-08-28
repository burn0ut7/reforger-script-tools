import { execFile } from "node:child_process";
import * as path from "node:path";
import { diagnostic } from "../../diagnostics/diagnostics";
import { workbenchWinePrefixArguments } from "../../extensionConfig/workbench";

const defaultGetStatusDeadlineMs = 1_500;
const defaultValidateScriptsDeadlineMs = 120_000;
const defaultGetLoadedAddonGraphDeadlineMs = 5_000;
const maximumResponseBytes = 4 * 1024 * 1024;

export interface WorkbenchEndpoint {
  host: string;
  port: number;
}

export interface WorkbenchGatewayOptions {
  enabled: boolean;
  endpoint: WorkbenchEndpoint;
  serverPath?: Promise<string | undefined>;
  deadlines?: Partial<WorkbenchGatewayDeadlines>;
  record?: (record: WorkbenchGatewayDiagnosticRecord) => void;
  onNetApiFailure?: (diagnosis: WorkbenchNetApiFailureDiagnosis) => void;
}

export interface WorkbenchGatewayDeadlines {
  getStatusMs: number;
  validateScriptsMs: number;
  getLoadedAddonGraphMs: number;
}

export interface WorkbenchGatewayDiagnosticRecord {
  capability: "getStatus" | "validateScripts" | "getLoadedAddonGraph";
  outcome: "success" | "compiler-findings" | WorkbenchGatewayFailureCategory;
  durationMs: number;
  timing?: WorkbenchPrivateApiTiming;
}

export interface WorkbenchPrivateApiTiming {
  /** Parent launch through Node's child-process spawn notification. */
  spawnEventMs?: number;
  /** Parent launch until the gateway begins writing its JSON response. */
  firstResponseByteMs?: number;
  /** Parent launch through the child's exit event. */
  exitEventMs?: number;
  /** Parent launch through stdio closure. */
  closeEventMs?: number;
  /** Spawn notification through child exit; this is the child lifetime seen by Node. */
  childLifetimeMs?: number;
  /** Parent launch through the execFile completion callback. */
  callbackMs?: number;
  controllerSetupMs?: number;
  commandMs?: number;
  request?: {
    lockWaitMs: number;
    connectMs: number;
    writeMs: number;
    responseHeaderMs: number;
    responseBodyMs: number;
    decodeMs: number;
    totalMs: number;
  };
}

export interface WorkbenchStatus {
  isRunning: boolean;
  scriptsCompiled: boolean;
}

export interface WorkbenchLogRead {
  source: string;
  path?: string;
  lines: string[];
  markers: Array<{ kind: string; lineIndex: number }>;
  truncated: boolean;
}

export type WorkbenchNetApiFailureDiagnosis = "bridge-inactive";

export type WorkbenchValidationProfile = "WORKBENCH";

export interface WorkbenchDiagnosticLocation {
  file: string;
  fileAbs?: string;
  addon?: string;
  line: number;
}

export interface WorkbenchCompilerDiagnostic {
  severity: "error" | "warning";
  message: string;
  location: WorkbenchDiagnosticLocation;
}

export interface WorkbenchValidationResult {
  profile: WorkbenchValidationProfile;
  success: boolean;
  diagnostics: WorkbenchCompilerDiagnostic[];
}

export interface WorkbenchLoadedAddon {
  guid: string;
  id: string;
  title: string;
  sourceRoot: string;
}

export interface WorkbenchLoadedAddonGraph {
  bridgeVersion: string;
  protocolVersion: 1;
  currentProjectFile?: string;
  addons: WorkbenchLoadedAddon[];
}

export interface WorkbenchIntegrationBootstrap {
  netApiEnabled: boolean;
  netApiWritePerformed: boolean;
  enfusionProtocolRegistered: boolean;
  enfusionProtocolWritePerformed: boolean;
  bridgeInstalled: boolean;
  bridgeVersion?: string;
  bridgeChanged: boolean;
  profileAvailable: boolean;
}

export interface WorkbenchProcessStatus {
  isOpen: boolean;
  processId?: number;
  projectPath?: string;
}

export type WorkbenchAvailability =
  | { kind: "disabled" }
  | { kind: "unavailable"; failure: WorkbenchGatewayFailure }
  | { kind: "ready" };

export type WorkbenchGatewayFailureCategory =
  | "consent-required"
  | "unavailable"
  | "timeout"
  | "protocol"
  | "unsupported"
  | "workbench-error";

export interface WorkbenchGatewayFailure {
  category: WorkbenchGatewayFailureCategory;
  recoveryHint: string;
}

export type WorkbenchGatewayResult<T> =
  { ok: true; value: T } | { ok: false; failure: WorkbenchGatewayFailure };

type WorkbenchPrivateApiResult<T> =
  | { ok: true; value: T; timing?: WorkbenchPrivateApiTiming }
  | {
      ok: false;
      failure: WorkbenchGatewayFailure;
      timing?: WorkbenchPrivateApiTiming;
    };

export type WorkbenchPrivateApiCommand =
  | "status"
  | "validate"
  | "loaded-addon-graph"
  | "read-logs"
  | "integration-status"
  | "bootstrap-integration"
  | "maintain-integration"
  | "process-status"
  | "launch-default"
  | "install-bridge";

export class WorkbenchGateway {
  private readonly options: WorkbenchGatewayOptions;
  private currentAvailability: WorkbenchAvailability;
  private failureDiagnosisInProgress = false;

  public constructor(options: WorkbenchGatewayOptions) {
    this.options = {
      ...options,
      endpoint: {
        ...options.endpoint,
        host: options.endpoint.host.trim(),
      },
    };
    this.currentAvailability = options.enabled
      ? { kind: "unavailable", failure: unavailableFailure() }
      : { kind: "disabled" };
  }

  public get availability(): WorkbenchAvailability {
    return this.currentAvailability.kind === "unavailable"
      ? {
          kind: "unavailable",
          failure: { ...this.currentAvailability.failure },
        }
      : { ...this.currentAvailability };
  }

  public async getStatus(): Promise<WorkbenchGatewayResult<WorkbenchStatus>> {
    const startedAt = Date.now();
    const result = await this.invokeStatus(
      deadline(this.options.deadlines?.getStatusMs, defaultGetStatusDeadlineMs),
    );
    if (!result.ok) {
      this.currentAvailability = this.options.enabled
        ? { kind: "unavailable", failure: result.failure }
        : { kind: "disabled" };
      await this.inspectFailedNetApiCall("getStatus", result.failure, result);
      this.record(
        "getStatus",
        result.failure.category,
        startedAt,
        result.timing,
      );
      return result;
    }
    const status = decodeStatus(result.value);
    if (!status.ok) {
      this.currentAvailability = {
        kind: "unavailable",
        failure: status.failure,
      };
      await this.inspectFailedNetApiCall("getStatus", status.failure, status);
      this.record(
        "getStatus",
        status.failure.category,
        startedAt,
        result.timing,
      );
      return status;
    }
    this.currentAvailability = { kind: "ready" };
    this.record("getStatus", "success", startedAt, result.timing);
    return status;
  }

  public async validateScripts(
    profile: WorkbenchValidationProfile,
  ): Promise<WorkbenchGatewayResult<WorkbenchValidationResult>> {
    const startedAt = Date.now();
    if (profile !== "WORKBENCH") {
      const result = failure(
        "unsupported",
        "Select the supported WORKBENCH validation profile.",
      );
      this.record("validateScripts", "unsupported", startedAt);
      return result;
    }
    const result = await this.invokeValidation(
      deadline(
        this.options.deadlines?.validateScriptsMs,
        defaultValidateScriptsDeadlineMs,
      ),
    );
    if (!result.ok) {
      this.noteFailure(result.failure);
      await this.inspectFailedNetApiCall("validateScripts", result.failure);
      this.record(
        "validateScripts",
        result.failure.category,
        startedAt,
        result.timing,
      );
      return result;
    }
    const validation = decodeValidation(profile, result.value);
    if (!validation.ok) {
      this.noteFailure(validation.failure);
      await this.inspectFailedNetApiCall("validateScripts", validation.failure);
      this.record(
        "validateScripts",
        validation.failure.category,
        startedAt,
        result.timing,
      );
      return validation;
    }
    this.currentAvailability = { kind: "ready" };
    this.record(
      "validateScripts",
      validation.value.success ? "success" : "compiler-findings",
      startedAt,
      result.timing,
    );
    return validation;
  }

  public async getLoadedAddonGraph(): Promise<
    WorkbenchGatewayResult<WorkbenchLoadedAddonGraph>
  > {
    const startedAt = Date.now();
    if (!this.options.enabled) {
      const result = failure(
        "unsupported",
        "Enable Workbench NET API integration in extension settings.",
      );
      this.record("getLoadedAddonGraph", "unsupported", startedAt);
      return result;
    }
    const endpointFailure = validateEndpoint(this.options.endpoint);
    if (endpointFailure) {
      this.record("getLoadedAddonGraph", endpointFailure.category, startedAt);
      return { ok: false, failure: endpointFailure };
    }
    const result = await invokeWorkbenchPrivateApi(
      this.options.serverPath ?? defaultDevelopmentServerPath(),
      this.options.endpoint,
      "loaded-addon-graph",
      deadline(
        this.options.deadlines?.getLoadedAddonGraphMs,
        defaultGetLoadedAddonGraphDeadlineMs,
      ),
    );
    if (!result.ok) {
      this.noteFailure(result.failure);
      await this.inspectFailedNetApiCall(
        "getLoadedAddonGraph",
        result.failure,
      );
      this.record(
        "getLoadedAddonGraph",
        result.failure.category,
        startedAt,
        result.timing,
      );
      return result;
    }
    const graph = decodeLoadedAddonGraph(result.value);
    if (!graph.ok) {
      this.noteFailure(graph.failure);
      await this.inspectFailedNetApiCall("getLoadedAddonGraph", graph.failure);
      this.record(
        "getLoadedAddonGraph",
        graph.failure.category,
        startedAt,
        result.timing,
      );
      return graph;
    }
    this.currentAvailability = { kind: "ready" };
    this.record("getLoadedAddonGraph", "success", startedAt, result.timing);
    return graph;
  }

  public async getProcessStatus(): Promise<
    WorkbenchGatewayResult<WorkbenchProcessStatus>
  > {
    const result = await invokeWorkbenchPrivateApi(
      this.options.serverPath ?? defaultDevelopmentServerPath(),
      this.options.endpoint,
      "process-status",
      defaultGetStatusDeadlineMs,
    );
    if (!result.ok) {
      return result;
    }
    return decodeProcessStatus(result.value);
  }

  public async readWorkbenchLogs(): Promise<
    WorkbenchGatewayResult<WorkbenchLogRead>
  > {
    const result = await invokeWorkbenchPrivateApi(
      this.options.serverPath ?? defaultDevelopmentServerPath(),
      this.options.endpoint,
      "read-logs",
      defaultGetStatusDeadlineMs,
    );
    if (!result.ok) {
      return result;
    }
    return decodeWorkbenchLogs(result.value);
  }

  public async diagnoseNetApiFailure(
    handler?: string,
    statusResult?: WorkbenchGatewayResult<WorkbenchStatus>,
  ): Promise<WorkbenchNetApiFailureDiagnosis | undefined> {
    diagnostic("workbenchNetApiDiagnosisStarted", {
      handler: handler ?? "any",
      statusProvided: statusResult !== undefined,
    });
    const status = statusResult ?? await this.getStatus();
    if (status.ok) {
      if (!status.value.isRunning) {
        diagnostic("workbenchNetApiDiagnosisWorkbenchNotRunning");
        return undefined;
      }
      diagnostic("workbenchNetApiDiagnosisStatus", {
        isRunning: true,
        scriptsCompiled: status.value.scriptsCompiled,
      });
    } else {
      diagnostic("workbenchNetApiDiagnosisStatusFailure", {
        category: status.failure.category,
      });
      const process = await this.getProcessStatus();
      if (!process.ok) {
        diagnostic("workbenchNetApiDiagnosisProcessFailure", {
          category: process.failure.category,
        });
        return undefined;
      }
      diagnostic("workbenchNetApiDiagnosisProcessStatus", {
        isOpen: process.value.isOpen,
      });
      if (!process.value.isOpen) {
        diagnostic("workbenchNetApiDiagnosisWorkbenchNotRunning");
        return undefined;
      }
    }
    const logs = await this.readWorkbenchLogs();
    if (!logs.ok) {
      diagnostic("workbenchNetApiDiagnosisLogFailure", {
        category: logs.failure.category,
      });
      return undefined;
    }
    const matched = workbenchLogReportsMissingHandler(logs.value.lines, handler);
    diagnostic("workbenchNetApiDiagnosisLogsRead", {
      source: logs.value.source,
      lineCount: logs.value.lines.length,
      markerCount: logs.value.markers.length,
      truncated: logs.value.truncated,
      missingHandlerMatched: matched,
    });
    return matched ? "bridge-inactive" : undefined;
  }

  private async inspectFailedNetApiCall(
    capability: string,
    failure: WorkbenchGatewayFailure,
    statusResult?: WorkbenchGatewayResult<WorkbenchStatus>,
  ): Promise<void> {
    diagnostic("workbenchNetApiFailureInspectionStarted", {
      capability,
      category: failure.category,
      statusProvided: statusResult !== undefined,
    });
    if (this.failureDiagnosisInProgress) {
      diagnostic("workbenchNetApiFailureInspectionSkipped", {
        capability,
        reason: "diagnosis-in-progress",
      });
      return;
    }
    this.failureDiagnosisInProgress = true;
    try {
      const diagnosis = await this.diagnoseNetApiFailure(undefined, statusResult);
      diagnostic("workbenchNetApiFailureInspectionCompleted", {
        capability,
        diagnosis: diagnosis ?? "none",
      });
      if (!diagnosis) {
        return;
      }
      try {
        this.options.onNetApiFailure?.(diagnosis);
        diagnostic("workbenchNetApiFailureNotificationDispatched", {
          capability,
          diagnosis,
        });
      } catch {
        diagnostic("workbenchNetApiFailureNotificationFailed", {
          capability,
          diagnosis,
        });
      }
    } finally {
      this.failureDiagnosisInProgress = false;
    }
  }

  private invokeStatus(
    deadlineMs: number,
  ): Promise<WorkbenchPrivateApiResult<unknown>> {
    if (!this.options.enabled) {
      return Promise.resolve(
        failure(
          "unsupported",
          "Enable Workbench NET API integration in extension settings.",
        ),
      );
    }
    const endpointFailure = validateEndpoint(this.options.endpoint);
    if (endpointFailure) {
      return Promise.resolve({ ok: false, failure: endpointFailure });
    }
    return invokeWorkbenchPrivateApi(
      this.options.serverPath ?? defaultDevelopmentServerPath(),
      this.options.endpoint,
      "status",
      deadlineMs,
    );
  }

  private invokeValidation(
    deadlineMs: number,
  ): Promise<WorkbenchPrivateApiResult<unknown>> {
    if (!this.options.enabled) {
      return Promise.resolve(
        failure(
          "unsupported",
          "Enable Workbench NET API integration in extension settings.",
        ),
      );
    }
    const endpointFailure = validateEndpoint(this.options.endpoint);
    if (endpointFailure) {
      return Promise.resolve({ ok: false, failure: endpointFailure });
    }
    return invokeWorkbenchPrivateApi(
      this.options.serverPath ?? defaultDevelopmentServerPath(),
      this.options.endpoint,
      "validate",
      deadlineMs,
    );
  }

  private noteFailure(gatewayFailure: WorkbenchGatewayFailure): void {
    this.currentAvailability = this.options.enabled
      ? { kind: "unavailable", failure: gatewayFailure }
      : { kind: "disabled" };
  }

  private record(
    capability: WorkbenchGatewayDiagnosticRecord["capability"],
    outcome: WorkbenchGatewayDiagnosticRecord["outcome"],
    startedAt: number,
    timing?: WorkbenchPrivateApiTiming,
  ): void {
    try {
      this.options.record?.({
        capability,
        outcome,
        durationMs: Date.now() - startedAt,
        ...(timing ? { timing } : {}),
      });
    } catch {
      // Host diagnostics must never affect a Gateway capability outcome.
    }
  }
}

function decodeStatus(value: unknown): WorkbenchGatewayResult<WorkbenchStatus> {
  if (
    !isRecord(value) ||
    typeof value.isRunning !== "boolean" ||
    typeof value.scriptsCompiled !== "boolean"
  ) {
    return failure(
      "protocol",
      "Restart Workbench and verify that its NET API is compatible.",
    );
  }
  return {
    ok: true,
    value: {
      isRunning: value.isRunning,
      scriptsCompiled: value.scriptsCompiled,
    },
  };
}

function decodeProcessStatus(
  value: unknown,
): WorkbenchGatewayResult<WorkbenchProcessStatus> {
  if (
    !isRecord(value) ||
    typeof value.isOpen !== "boolean" ||
    (value.processId !== undefined && typeof value.processId !== "number") ||
    (value.projectPath !== undefined && typeof value.projectPath !== "string")
  ) {
    return failure("protocol", "Restart Workbench and retry the request.");
  }
  return {
    ok: true,
    value: {
      isOpen: value.isOpen,
      ...(value.processId === undefined ? {} : { processId: value.processId }),
      ...(value.projectPath === undefined ? {} : { projectPath: value.projectPath }),
    },
  };
}

function decodeWorkbenchLogs(value: unknown): WorkbenchGatewayResult<WorkbenchLogRead> {
  if (
    !isRecord(value) ||
    typeof value.source !== "string" ||
    (value.path !== undefined && value.path !== null && typeof value.path !== "string") ||
    !Array.isArray(value.lines) ||
    !value.lines.every(line => typeof line === "string") ||
    !Array.isArray(value.markers) ||
    !value.markers.every(marker =>
      isRecord(marker) &&
      typeof marker.kind === "string" &&
      Number.isInteger(marker.lineIndex),
    ) ||
    typeof value.truncated !== "boolean"
  ) {
    return failure("protocol", "Restart Workbench and retry the request.");
  }
  return {
    ok: true,
    value: {
      source: value.source,
      ...(typeof value.path === "string" ? { path: value.path } : {}),
      lines: value.lines,
      markers: value.markers.map(marker => ({
        kind: marker.kind,
        lineIndex: marker.lineIndex,
      })),
      truncated: value.truncated,
    },
  };
}

export function workbenchLogReportsMissingHandler(
  lines: readonly string[],
  handler?: string,
): boolean {
  const marker = "Failed to call not existing Net API function '";
  return lines.some(line =>
    line.includes(marker) &&
    (handler === undefined || line.includes(`'${handler}'`)),
  );
}

function decodeValidation(
  profile: WorkbenchValidationProfile,
  value: unknown,
): WorkbenchGatewayResult<WorkbenchValidationResult> {
  if (
    !isRecord(value) ||
    value.profile !== profile ||
    typeof value.success !== "boolean" ||
    !Array.isArray(value.diagnostics) ||
    !value.diagnostics.every(isCompilerDiagnostic)
  ) {
    return failure(
      "protocol",
      "Restart Workbench and verify that its NET API is compatible.",
    );
  }
  return {
    ok: true,
    value: {
      profile,
      success: value.success,
      diagnostics: value.diagnostics,
    },
  };
}

function decodeLoadedAddonGraph(
  value: unknown,
): WorkbenchGatewayResult<WorkbenchLoadedAddonGraph> {
  if (
    !isRecord(value) ||
    typeof value.bridgeVersion !== "string" ||
    value.protocolVersion !== 1 ||
    (value.currentProjectFile !== undefined &&
      (typeof value.currentProjectFile !== "string" ||
        !path.isAbsolute(value.currentProjectFile))) ||
    !Array.isArray(value.addons) ||
    !value.addons.every(isLoadedAddon)
  ) {
    return failure(
      "protocol",
      "Reload Workbench scripts and verify the Reforger Script Tools bridge version.",
    );
  }
  const identities = new Set<string>();
  for (const addon of value.addons) {
    const identity = addon.guid.toUpperCase();
    if (identities.has(identity)) {
      return failure(
        "protocol",
        "Reload Workbench scripts and retry the loaded add-on graph request.",
      );
    }
    identities.add(identity);
  }
  return {
    ok: true,
    value: {
      bridgeVersion: value.bridgeVersion,
      protocolVersion: 1,
      ...(value.currentProjectFile === undefined
        ? {}
        : { currentProjectFile: value.currentProjectFile }),
      addons: value.addons.map((addon) => ({ ...addon })),
    },
  };
}

function isLoadedAddon(value: unknown): value is WorkbenchLoadedAddon {
  return (
    isRecord(value) &&
    typeof value.guid === "string" &&
    /^[0-9a-f]{16}$/i.test(value.guid) &&
    typeof value.id === "string" &&
    value.id.length > 0 &&
    typeof value.title === "string" &&
    value.title.length > 0 &&
    typeof value.sourceRoot === "string" && path.isAbsolute(value.sourceRoot)
  );
}

function isCompilerDiagnostic(
  value: unknown,
): value is WorkbenchCompilerDiagnostic {
  if (
    !isRecord(value) ||
    (value.severity !== "error" && value.severity !== "warning") ||
    typeof value.message !== "string" ||
    !isRecord(value.location)
  ) {
    return false;
  }
  const location = value.location;
  return (
    typeof location.file === "string" &&
    Number.isInteger(location.line) &&
    (location.fileAbs === undefined || typeof location.fileAbs === "string") &&
    (location.addon === undefined || typeof location.addon === "string")
  );
}

export async function invokeWorkbenchPrivateApi(
  serverPath: Promise<string | undefined>,
  endpoint: WorkbenchEndpoint,
  action: WorkbenchPrivateApiCommand,
  deadlineMs: number,
): Promise<WorkbenchPrivateApiResult<unknown>> {
  diagnostic("workbenchNetApiPrivateCallStarted", {
    action,
    endpointPort: endpoint.port,
    deadlineMs,
  });
  const endpointFailure = validateEndpoint(endpoint);
  if (endpointFailure) {
    diagnostic("workbenchNetApiPrivateCallRejected", {
      action,
      category: endpointFailure.category,
    });
    return { ok: false, failure: endpointFailure };
  }
  const executable = await serverPath;
  if (!executable) {
    diagnostic("workbenchNetApiPrivateCallRejected", {
      action,
      category: "unavailable",
      reason: "server-executable-unavailable",
    });
    return failure("unavailable", "Restart the extension and retry.");
  }
  return new Promise((resolve) => {
    const invokedAt = Date.now();
    let spawnedAt: number | undefined;
    let firstResponseByteAt: number | undefined;
    let exitedAt: number | undefined;
    let closedAt: number | undefined;
    const child = execFile(
      executable,
      [
        "workbench-api",
        action,
        "--host",
        endpoint.host,
        "--port",
        String(endpoint.port),
        "--deadline-ms",
        String(deadlineMs),
        ...workbenchWinePrefixArguments(),
      ],
      {
        timeout: deadlineMs + 500,
        maxBuffer: maximumResponseBytes,
        windowsHide: true,
      },
      (error, stdout) => {
        const processTiming = childLifecycleTiming(
          invokedAt,
          spawnedAt,
          firstResponseByteAt,
          exitedAt,
          closedAt,
          Date.now(),
        );
        if (error) {
          diagnostic("workbenchNetApiPrivateCallCompleted", {
            action,
            outcome: "process-error",
            category:
              error.killed || error.code === "ETIMEDOUT"
                ? "timeout"
                : "unavailable",
            killed: error.killed === true,
            errorCode: typeof error.code === "string" ? error.code : undefined,
            stdoutBytes: Buffer.byteLength(stdout, "utf8"),
            callbackMs: processTiming.callbackMs,
          });
          resolve({
            ...failure(
              error.killed || error.code === "ETIMEDOUT"
                ? "timeout"
                : "unavailable",
              "Restart Workbench and retry the request.",
            ),
            timing: processTiming,
          });
          return;
        }
        try {
          const result = JSON.parse(stdout) as {
            ok: boolean;
            value?: unknown;
            failure?: { category?: WorkbenchGatewayFailureCategory };
            timing?: unknown;
          };
          const timing = {
            ...processTiming,
            ...decodePrivateApiTiming(result.timing),
          };
          if (result.ok) {
            diagnostic("workbenchNetApiPrivateCallCompleted", {
              action,
              outcome: "success",
              stdoutBytes: Buffer.byteLength(stdout, "utf8"),
              callbackMs: timing.callbackMs,
            });
            resolve({ ok: true, value: result.value, timing });
            return;
          }
          diagnostic("workbenchNetApiPrivateCallCompleted", {
            action,
            outcome: "gateway-failure",
            category: result.failure?.category ?? "protocol",
            stdoutBytes: Buffer.byteLength(stdout, "utf8"),
            callbackMs: timing.callbackMs,
          });
          resolve({
            ...failure(
              result.failure?.category ?? "protocol",
              "Review Workbench state and retry the operation.",
            ),
            timing,
          });
        } catch {
          diagnostic("workbenchNetApiPrivateCallCompleted", {
            action,
            outcome: "invalid-json",
            category: "protocol",
            stdoutBytes: Buffer.byteLength(stdout, "utf8"),
            callbackMs: processTiming.callbackMs,
          });
          resolve({
            ...failure("protocol", "Restart Workbench and retry the request."),
            timing: processTiming,
          });
        }
      },
    );
    child.once("spawn", () => {
      spawnedAt = Date.now();
    });
    child.stdout?.once("data", () => {
      firstResponseByteAt ??= Date.now();
    });
    child.once("exit", () => {
      exitedAt = Date.now();
    });
    child.once("close", () => {
      closedAt = Date.now();
    });
  });
}

function childLifecycleTiming(
  invokedAt: number,
  spawnedAt: number | undefined,
  firstResponseByteAt: number | undefined,
  exitedAt: number | undefined,
  closedAt: number | undefined,
  callbackAt: number,
): Pick<
  WorkbenchPrivateApiTiming,
  | "spawnEventMs"
  | "firstResponseByteMs"
  | "exitEventMs"
  | "closeEventMs"
  | "childLifetimeMs"
  | "callbackMs"
> {
  return {
    ...(spawnedAt === undefined ? {} : { spawnEventMs: spawnedAt - invokedAt }),
    ...(firstResponseByteAt === undefined
      ? {}
      : { firstResponseByteMs: firstResponseByteAt - invokedAt }),
    ...(exitedAt === undefined ? {} : { exitEventMs: exitedAt - invokedAt }),
    ...(closedAt === undefined ? {} : { closeEventMs: closedAt - invokedAt }),
    ...(spawnedAt === undefined || exitedAt === undefined
      ? {}
      : { childLifetimeMs: exitedAt - spawnedAt }),
    callbackMs: callbackAt - invokedAt,
  };
}

function decodePrivateApiTiming(value: unknown): WorkbenchPrivateApiTiming {
  if (!isRecord(value)) {
    return {};
  }
  const requestValue = isRecord(value.request) ? value.request : undefined;
  const request =
    requestValue &&
    [
      "lockWaitMs",
      "connectMs",
      "writeMs",
      "responseHeaderMs",
      "responseBodyMs",
      "decodeMs",
      "totalMs",
    ].every(
      (key) =>
        typeof requestValue[key] === "number" &&
        Number.isFinite(requestValue[key]),
    )
      ? {
          lockWaitMs: requestValue.lockWaitMs as number,
          connectMs: requestValue.connectMs as number,
          writeMs: requestValue.writeMs as number,
          responseHeaderMs: requestValue.responseHeaderMs as number,
          responseBodyMs: requestValue.responseBodyMs as number,
          decodeMs: requestValue.decodeMs as number,
          totalMs: requestValue.totalMs as number,
        }
      : undefined;
  return {
    ...(typeof value.controllerSetupMs === "number" &&
    Number.isFinite(value.controllerSetupMs)
      ? { controllerSetupMs: value.controllerSetupMs }
      : {}),
    ...(typeof value.commandMs === "number" && Number.isFinite(value.commandMs)
      ? { commandMs: value.commandMs }
      : {}),
    ...(request ? { request } : {}),
  };
}

function defaultDevelopmentServerPath(): Promise<string | undefined> {
  return Promise.resolve(
    path.resolve(
      __dirname,
      "..",
      "..",
      "..",
      "server",
      "target",
      "debug",
      process.platform === "win32"
        ? "reforger_language_server.exe"
        : "reforger_language_server",
    ),
  );
}

function validateEndpoint(
  endpoint: WorkbenchEndpoint,
): WorkbenchGatewayFailure | undefined {
  if (!isLoopbackHost(endpoint.host)) {
    return {
      category: "unsupported",
      recoveryHint: "Configure a loopback Workbench host such as 127.0.0.1.",
    };
  }
  if (
    !Number.isInteger(endpoint.port) ||
    endpoint.port < 1 ||
    endpoint.port > 65_535
  ) {
    return {
      category: "unsupported",
      recoveryHint: "Configure a Workbench NET API port from 1 through 65535.",
    };
  }
  return undefined;
}

function isLoopbackHost(host: string): boolean {
  const normalized = host.trim().toLowerCase();
  if (normalized === "::1") {
    return true;
  }
  const parts = normalized.split(".");
  return (
    parts.length === 4 &&
    parts[0] === "127" &&
    parts.every((part) => /^\d{1,3}$/.test(part) && Number(part) <= 255)
  );
}

function deadline(configured: number | undefined, defaultMs: number): number {
  return configured !== undefined &&
    Number.isFinite(configured) &&
    configured > 0
    ? configured
    : defaultMs;
}

function unavailableFailure(): WorkbenchGatewayFailure {
  return {
    category: "unavailable",
    recoveryHint: "Start Workbench with NET API enabled, then retry.",
  };
}

function failure(
  category: WorkbenchGatewayFailureCategory,
  recoveryHint: string,
): WorkbenchGatewayResult<never> {
  return { ok: false, failure: { category, recoveryHint } };
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
