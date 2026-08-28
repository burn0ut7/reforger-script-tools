import * as fs from "fs/promises";
import * as path from "path";
import * as vscode from "vscode";
import {
  CloseAction,
  ErrorAction,
  LanguageClient,
  type ErrorHandler,
  type LanguageClientOptions,
  NotificationType,
  type ServerOptions,
  State,
  TransportKind,
} from "vscode-languageclient/node";
import {
  bracketColoringConfig,
  type BracketColoringMode,
  getBracketColoringMode,
} from "../extensionConfig/bracketColoring";
import {
  diagnostic,
  diagnosticsEnabled,
  languageServerDiagnosticPath,
} from "../diagnostics/diagnostics";
import {
  languageClientCrashHandling,
  languageClientCompletion,
  languageClientCommands,
  languageClientDocumentSelector,
  languageClientIndexCache,
  languageClientIds,
  languageClientLanguage,
  languageClientLogs,
  languageClientNotifications,
  languageClientRequests,
  languageClientSchemes,
} from "../extensionConfig/languageClient";
import {
  clearConfirmedLoadedAddonSourceInventory,
  confirmLoadedAddonSourceInventory,
  writeLoadedAddonSourceInventory,
} from "../gameData/localSourceInventory";
import {
  workbenchConfig,
  workbenchDefaults,
  workbenchWinePrefixArguments,
  externalIndexModes,
  type ExternalIndexMode,
} from "../extensionConfig/workbench";
import { WorkbenchGateway } from "../workbenchNetApi/gateway/workbenchGateway";
import {
  resetWorkbenchFailureNotification,
  updateWorkbenchFailureNotification,
} from "../workbenchNetApi/workbenchFailureNotification";
import { registerHtmlHoverBridge } from "./hoverBridge";
import {
  discoverWorkspaceScriptRoots,
  discoverWorkspaceProjectFiles,
  registerWorkspaceScriptWatchBridge,
} from "./workspaceWatchBridge";
import { registerDebugCommandBridge } from "./debugCommandBridge";
import {
  completionItemCount,
  completionPresentationMetadata,
  createCompletionMiddleware,
  isCompletionListIncomplete,
} from "./completionMiddleware";
import {
  completionPresentationObservationForDocument,
  completionUiMiddlewareCallbacks,
  registerCompletionUiBridge,
} from "./completionUiBridge";
import { openSymbolLocation } from "./symbolLocationBridge";
import {
  disposeDevelopmentServerWatchBridge,
  registerDevelopmentServerWatchBridge,
} from "./developmentServerWatchBridge";
import { registerBlockCommentPair } from "./typingAssistTransactionBridge";
import { registerControlHeaderEnter } from "./controlHeaderEnterBridge";
import { registerActiveScopeDelimiterBridge } from "./activeScopeDelimiterBridge";
import {
  applyBracketColoringEditorMode,
  bracketColoringServerArguments,
  usesCustomScopeDelimiterPresentation,
} from "./bracketColoringBridge";
import { RestartCoordinator } from "./restartCoordinator";
import { resolveLanguageServerPath } from "./serverPath";
import { registerSemanticTokenTimingBridge } from "./semanticTokenTimingBridge";
import { registerSemanticTokenBoundaryGuardBridge } from "./semanticTokenBoundaryGuardBridge";

export { blockCommentPairPosition } from "./typingAssistBridge";
export { ifSpaceCommitContractFromCommandArguments } from "./completionUiBridge";

let client: LanguageClient | undefined;
let clientDisposables: vscode.Disposable[] = [];
let refreshWorkbenchGraph: (() => Promise<void>) | undefined;
let languageClientStartGeneration = 0;
const restartCoordinator = new RestartCoordinator();
let initialStartup: Promise<void> | undefined;
const workspaceWatcherDebounceMs = 250;
let startupTimingSessionStartMs = Date.now();
let firstDocumentOpenTimingLogged = false;
let firstSemanticTokenTimingLogged = false;

export async function provideLanguageServerSemanticTokens(
	document: vscode.TextDocument,
	token: vscode.CancellationToken,
): Promise<vscode.SemanticTokens | undefined> {
	const activeClient = client;
	if (!activeClient || activeClient.state !== State.Running) {
		return undefined;
	}
	const response = await activeClient.sendRequest<{ data?: number[] }>(
		'textDocument/semanticTokens/full',
		{ textDocument: { uri: document.uri.toString() } },
		token,
	);
	if (!Array.isArray(response?.data)) {
		return undefined;
	}
	return new vscode.SemanticTokens(new Uint32Array(response.data));
}

export interface LanguageServerPreviewContext {
	startLine: number;
	endLine: number;
	kind: string;
	truncated: boolean;
}

export async function provideLanguageServerPreviewContext(
	document: vscode.TextDocument,
	line: number,
	token: vscode.CancellationToken,
): Promise<LanguageServerPreviewContext | undefined> {
	const activeClient = client;
	if (!activeClient || activeClient.state !== State.Running) {
		return undefined;
	}
	const response = await activeClient.sendRequest<unknown>(
		languageClientRequests.previewContext,
		{
			textDocument: { uri: document.uri.toString() },
			position: { line, character: 0 },
		},
		token,
	);
	if (!isLanguageServerPreviewContext(response, document.lineCount, line)) {
		return undefined;
	}
	return response;
}

function isLanguageServerPreviewContext(
	value: unknown,
	lineCount: number,
	requestedLine: number,
): value is LanguageServerPreviewContext {
	if (!value || typeof value !== 'object') {
		return false;
	}
	const candidate = value as Partial<LanguageServerPreviewContext>;
	return Number.isInteger(candidate.startLine)
		&& Number.isInteger(candidate.endLine)
		&& typeof candidate.kind === 'string'
		&& typeof candidate.truncated === 'boolean'
		&& (candidate.startLine ?? -1) >= 0
		&& (candidate.endLine ?? lineCount) < lineCount
		&& (candidate.startLine ?? requestedLine + 1) <= requestedLine
		&& (candidate.endLine ?? requestedLine - 1) >= requestedLine;
}

interface ExternalIndexProgressParams {
  phase: string;
  status?: string;
  gameDataFiles?: number;
}

type ExternalIndexProgress = vscode.Progress<{
  message?: string;
  increment?: number;
}>;

interface ExternalIndexProgressSession {
  progress: ExternalIndexProgress;
}

export interface GameDataRefreshOptions {
  showProgress?: boolean;
}

type GameDataProgressPresenter = (
  task: (progress: ExternalIndexProgress) => Promise<void>,
) => Thenable<void>;

let activeExternalIndexProgressSession:
  ExternalIndexProgressSession | undefined;

export function beginLanguageClientStartupTimingSession(
  startedAtMs = Date.now(),
): void {
  startupTimingSessionStartMs = startedAtMs;
  firstDocumentOpenTimingLogged = false;
  firstSemanticTokenTimingLogged = false;
}

export function languageClientStartupElapsedMs(nowMs = Date.now()): number {
  return Math.max(0, nowMs - startupTimingSessionStartMs);
}

export function logLanguageClientStartupTiming(
  _context: vscode.ExtensionContext,
  event: string,
  fields: Record<string, string | number | boolean | undefined> = {},
): void {
  const elapsedMs = languageClientStartupElapsedMs();
  diagnostic(`startup.${event}`, {
    elapsedMs,
    ...sanitizeDiagnosticFields(fields),
  });
}

function sanitizeDiagnosticFields(
  fields: Record<string, string | number | boolean | undefined>,
): Record<string, string | number | boolean | undefined> {
  const safe: Record<string, string | number | boolean | undefined> = {};
  for (const [key, value] of Object.entries(fields)) {
    if (
      !key.toLowerCase().includes("path") &&
      key !== "uri" &&
      key !== "serverPath" &&
      key !== "message"
    ) {
      safe[key] = value;
    }
  }
  return safe;
}

export function registerLanguageClientFeatures(
	context: vscode.ExtensionContext,
	workbenchReady?: Promise<boolean>,
	workbenchStartupGate?: Promise<boolean>,
): (options?: GameDataRefreshOptions) => Promise<void> {
  logLanguageClientStartupTiming(context, "languageClientRegistrationStart");
  const outputChannel = vscode.window.createOutputChannel(
    languageClientIds.name,
    { log: true },
  );
  const debugOutputChannel = vscode.window.createOutputChannel(
    languageClientIds.debugOutputName,
  );
  const completionDebugOutputChannel = vscode.window.createOutputChannel(
    languageClientIds.completionDebugOutputName,
  );
  context.subscriptions.push(outputChannel);
  context.subscriptions.push(debugOutputChannel);
  context.subscriptions.push(completionDebugOutputChannel);
  context.subscriptions.push(...registerControlHeaderEnter(() => client));
  context.subscriptions.push(registerBlockCommentPair(() => client));
  context.subscriptions.push(...registerCompletionUiBridge());
  context.subscriptions.push(
    ...registerDebugCommandBridge(
      context,
      () => client,
      debugOutputChannel,
      completionDebugOutputChannel,
      completionPresentationObservationForDocument,
    ),
  );
  context.subscriptions.push(
    vscode.commands.registerCommand(
      languageClientCommands.openSymbolLocation,
      (args: unknown) => openSymbolLocation(args),
    ),
  );
  context.subscriptions.push(registerFirstDocumentOpenTiming(context));
  context.subscriptions.push(
    vscode.workspace.registerTextDocumentContentProvider(languageClientSchemes.packedSource, {
      provideTextDocumentContent: async (uri) => {
        if (!client) {
          throw new Error("Reforger language server is not ready.");
        }
        return client.sendRequest<string>(
          languageClientRequests.readPackSource,
          { uri: uri.toString() },
        );
      },
    }),
  );
  context.subscriptions.push(
    vscode.workspace.onDidChangeConfiguration((event) => {
      const setting = `${bracketColoringConfig.section}.${bracketColoringConfig.setting}`;
      if (event.affectsConfiguration(setting)) {
        void restartLanguageClient(
          context,
          outputChannel,
          "bracket coloring changed",
          true,
          workbenchReady,
        );
      }
      if (event.affectsConfiguration(`${workbenchConfig.section}.${workbenchConfig.settings.externalIndexMode}`)) {
        void restartLanguageClient(
          context,
          outputChannel,
          "external index mode changed",
          false,
          workbenchReady,
        );
      }
    }),
  );

  const bracketColoring = getBracketColoringMode();
  const startup = synchronizeBracketColoringEditorMode(
    bracketColoring,
    outputChannel,
  ).then(() => runAfterWorkbenchStartupGate(workbenchStartupGate, () => vscode.window.withProgress(
    {
      location: vscode.ProgressLocation.Window,
      title: 'Reforger: Indexing loaded add-ons',
      cancellable: false,
    },
    progress => startLanguageClient(
      context,
      outputChannel,
      bracketColoring,
      progress,
      workbenchReady,
    ),
  )));
  initialStartup = startup;
  void startup.finally(() => {
    if (initialStartup === startup) {
      initialStartup = undefined;
    }
  });
  logLanguageClientStartupTiming(context, "languageClientRegistrationEnd");
  return async (options) => {
    await runWithGameDataProgress(options, async () => {
      if (refreshWorkbenchGraph) {
        await refreshWorkbenchGraph();
      } else {
        await restartLanguageClient(
          context,
          outputChannel,
          "game-data source changed",
          true,
          workbenchReady,
        );
      }
    });
  };
}

export async function runWithGameDataProgress(
  options: GameDataRefreshOptions | undefined,
  task: () => Promise<void>,
  present: GameDataProgressPresenter = callback => vscode.window.withProgress(
    gameDataProgressOptions(),
    callback,
  ),
): Promise<void> {
  if (options?.showProgress === false) {
    await task();
    return;
  }
  await present(async progress => {
    const session = { progress };
    activeExternalIndexProgressSession = session;
    try {
      await task();
    } finally {
      if (activeExternalIndexProgressSession === session) {
        activeExternalIndexProgressSession = undefined;
      }
    }
  });
}

export function gameDataProgressOptions(): vscode.ProgressOptions {
  return {
    location: vscode.ProgressLocation.Window,
    title: "Reforger game data",
    cancellable: false,
  };
}

export function runAfterWorkbenchStartupGate<T>(
	gate: Promise<boolean> | undefined,
	task: () => PromiseLike<T> | T,
): Promise<T> {
	return Promise.resolve(gate).then(() => task());
}

function registerFirstDocumentOpenTiming(
  context: vscode.ExtensionContext,
): vscode.Disposable {
  for (const document of vscode.workspace.textDocuments) {
    if (document.languageId === languageClientLanguage.id) {
      logFirstDocumentOpened(context, document, "alreadyOpen");
      break;
    }
  }

  return vscode.workspace.onDidOpenTextDocument((document) => {
    if (document.languageId === languageClientLanguage.id) {
      logFirstDocumentOpened(context, document, "didOpenEvent");
    }
  });
}

function logFirstDocumentOpened(
  context: vscode.ExtensionContext,
  document: vscode.TextDocument,
  source: string,
): void {
  if (firstDocumentOpenTimingLogged) {
    return;
  }
  firstDocumentOpenTimingLogged = true;
  logLanguageClientStartupTiming(context, "firstDocumentOpened", {
    source,
    uri: document.uri.toString(),
    languageId: document.languageId,
    lineCount: document.lineCount,
    byteLength: Buffer.byteLength(document.getText(), "utf8"),
  });
}

export async function deactivateLanguageClient(): Promise<void> {
  diagnostic("languageClient.deactivate");
  languageClientStartGeneration += 1;
  disposeClientDisposables();
  disposeDevelopmentServerWatchBridge();
  resetWorkbenchFailureNotification();
  const activeClient = client;
  client = undefined;
  if (activeClient) {
    await activeClient.stop();
  }
}

async function startLanguageClient(
	context: vscode.ExtensionContext,
	outputChannel: vscode.LogOutputChannel,
	bracketColoring: BracketColoringMode,
	externalIndexProgress?: ExternalIndexProgress,
	workbenchReady?: Promise<boolean>,
): Promise<void> {
  const startGeneration = ++languageClientStartGeneration;
  logLanguageClientStartupTiming(context, "languageClientStartBegin");
  const serverPath = await resolveLanguageServerPath(context);
  if (!serverPath) {
    outputChannel.appendLine(
      "Language server binary was not found. Run npm run build-server during development.",
    );
    logLanguageClientStartupTiming(context, "languageClientStartAborted", {
      reason: "serverBinaryNotFound",
    });
    return;
  }
  if (startGeneration !== languageClientStartGeneration) {
    return;
  }
  logLanguageClientStartupTiming(context, "languageServerPathResolved", {
    serverPath,
    extensionMode: extensionModeName(context.extensionMode),
  });
  registerDevelopmentServerWatchBridge(context, serverPath, () => {
    void restartLanguageClient(
      context,
      outputChannel,
      "development language-server binary changed",
      true,
      workbenchReady,
    ).catch((error) => {
      const message = error instanceof Error ? error.message : String(error);
      outputChannel.appendLine(
        `Development language-server restart failed: ${message}`,
      );
    });
  });

  const externalIndexMode = readExternalIndexMode();
  clearConfirmedLoadedAddonSourceInventory();
  const currentSourceInventory = resolveCurrentWorkbenchAddonInventory(
    context,
    serverPath,
    outputChannel,
    externalIndexMode,
    workbenchReady,
  );
  const workspaceScriptRootsPromise = discoverWorkspaceScriptRoots();
  const workspaceProjectFilesPromise = externalIndexMode === 'loaded'
    ? discoverWorkspaceProjectFiles()
    : Promise.resolve([] as string[]);
  const workspaceProjectFiles = await workspaceProjectFilesPromise;
  const serverArgs = [
    "--addon-index-storage",
    path.join(
      context.globalStorageUri.fsPath,
      languageClientIndexCache.rootFolder,
    ),
    "--external-index-mode",
    externalIndexMode,
    ...workbenchWinePrefixArguments(),
    ...bracketColoringServerArguments(bracketColoring),
  ];
  if (diagnosticsEnabled()) {
    const logsRoot = path.join(
      context.globalStorageUri.fsPath,
      languageClientLogs.rootFolder,
    );
    await fs.mkdir(logsRoot, { recursive: true });
    serverArgs.push(
      "--log",
      path.join(logsRoot, languageClientLogs.serverLogFile),
    );
  }
  const diagnosticPath = languageServerDiagnosticPath(context);
  if (diagnosticPath) {
    serverArgs.push("--diagnostic-log", diagnosticPath);
  }
  const workspaceScriptRoots = await workspaceScriptRootsPromise;
  if (startGeneration !== languageClientStartGeneration) {
    return;
  }
  for (const root of workspaceScriptRoots) {
    serverArgs.push("--workspace-scripts", root);
  }
  if (externalIndexMode === 'loaded') {
    for (const projectFile of workspaceProjectFiles) {
      serverArgs.push("--dependency-project", projectFile);
    }
  }
  logLanguageClientStartupTiming(context, "languageServerArgumentsReady", {
    workbenchGraphDeliveryPending: externalIndexMode === 'loaded',
    externalIndexMode,
    workspaceScriptRoots: workspaceScriptRoots.length,
    dependencyProjectFiles: workspaceProjectFiles.length,
    serverArgs: serverArgs.length,
    bracketColoring,
  });

  const serverOptions: ServerOptions = {
    run: {
      command: serverPath,
      args: serverArgs,
      transport: TransportKind.stdio,
    },
    debug: {
      command: serverPath,
      args: serverArgs,
      transport: TransportKind.stdio,
    },
  };

  const semanticTokenTiming = registerSemanticTokenTimingBridge();
  clientDisposables.push(semanticTokenTiming);
  const semanticTokenBoundaryGuard = registerSemanticTokenBoundaryGuardBridge();
  clientDisposables.push(semanticTokenBoundaryGuard);
  const clientOptions: LanguageClientOptions = {
    documentSelector: [...languageClientDocumentSelector],
    outputChannel,
    errorHandler: createLanguageServerErrorHandler(),
    markdown: {
      isTrusted: true,
      supportHtml: true,
    },
    middleware: {
      provideHover: () => null,
      ...createCompletionMiddleware(completionUiMiddlewareCallbacks),
      provideDocumentSemanticTokens: async (document, token, next) => {
        const timing = semanticTokenTiming.start(document);
        const startedAt = Date.now();
        const version = document.version;
        try {
          const result = await next(document, token);
          if (!token.isCancellationRequested && result) {
            semanticTokenBoundaryGuard.update(document, version, result);
          }
          semanticTokenTiming.complete(
            timing,
            "ok",
            token.isCancellationRequested,
          );
          logFirstSemanticTokenResponse(context, document, startedAt, "ok");
          return result;
        } catch (error) {
          semanticTokenTiming.complete(
            timing,
            "error",
            token.isCancellationRequested,
          );
          logFirstSemanticTokenResponse(
            context,
            document,
            startedAt,
            "error",
            error,
          );
          throw error;
        }
      },
    },
  };

  logLanguageClientStartupTiming(context, "languageClientCreateStart");
  if (startGeneration !== languageClientStartGeneration) {
    return;
  }
  const nextClient = new LanguageClient(
    languageClientIds.id,
    languageClientIds.name,
    serverOptions,
    clientOptions,
  );
  client = nextClient;
  const activeClient = nextClient;
  logLanguageClientStartupTiming(context, "languageClientCreated");
  // The external index is asynchronous, but it is still part of a usable
  // language-tooling startup. Monitor every launch so support diagnostics can
  // distinguish server initialization from index readiness. A refresh also
  // supplies UI progress and waits for this same authoritative completion.
  const externalIndexMonitor = monitorExternalIndexProgress(
    context,
    client,
    externalIndexProgress,
  );
  clientDisposables.push(externalIndexMonitor.disposable);

  try {
    externalIndexProgress?.report({ message: "Starting language server" });
    logLanguageClientStartupTiming(
      context,
      "languageServerProcessSpawnRequested",
      {
        serverPath,
        transport: "stdio",
      },
    );
    await client.start();
    if (startGeneration !== languageClientStartGeneration || client !== activeClient) {
      return;
    }
    refreshWorkbenchGraph = async () => {
      if (externalIndexMode === 'all' || externalIndexMode === 'none') {
        return;
      }
      const inventoryPath = await resolveWorkbenchLoadedAddonInventory(
        context,
        serverPath,
        outputChannel,
      );
      if (inventoryPath && client === activeClient) {
        const reconciliation = externalIndexMonitor.waitForNextCompletion(
          activeExternalIndexProgressSession?.progress,
          "workbench-reconciliation",
        );
        activeClient.sendNotification(languageClientNotifications.loadedAddonGraph, {
          inventoryPath,
        });
        await reconciliation;
        if (client === activeClient) {
          confirmLoadedAddonSourceInventory(inventoryPath);
        }
      }
    };
    void currentSourceInventory.then(async (inventoryPath) => {
      if (!inventoryPath) {
        return;
      }
      if (client !== activeClient) {
        return;
      }
      const reconciliation = externalIndexMonitor.waitForNextCompletion(
        undefined,
        "workbench-reconciliation",
      );
      activeClient.sendNotification(languageClientNotifications.loadedAddonGraph, {
        inventoryPath,
      });
      logLanguageClientStartupTiming(
        context,
        "workbenchLoadedAddonGraphDelivered",
      );
      await reconciliation;
      if (client === activeClient) {
        confirmLoadedAddonSourceInventory(inventoryPath);
      }
    });
    diagnostic("languageClient.started", {
      workspaceScriptRoots: workspaceScriptRoots.length,
    });
    logLanguageClientStartupTiming(
      context,
      "languageServerInitializeResponse",
      {
        serverPath,
      },
    );
    outputChannel.appendLine(`Language server started: ${serverPath}`);
    if (workspaceScriptRoots.length > 0) {
      outputChannel.appendLine(
        `Workspace script roots: ${workspaceScriptRoots.join("; ")}`,
      );
    }
    clientDisposables.push(registerHtmlHoverBridge(client, outputChannel));
    clientDisposables.push(
      ...registerWorkspaceScriptWatchBridge(client, outputChannel),
    );
    if (usesCustomScopeDelimiterPresentation(bracketColoring)) {
      clientDisposables.push(registerActiveScopeDelimiterBridge(client));
    }
    if (externalIndexProgress) {
      await externalIndexMonitor.completion;
    }
  } catch (error) {
    externalIndexMonitor?.disposable.dispose();
    const message = error instanceof Error ? error.message : String(error);
    outputChannel.appendLine(`Language server failed to start: ${message}`);
    logLanguageClientStartupTiming(context, "languageClientStartFailed", {
      message,
    });
    vscode.window.showWarningMessage(
      `Reforger language server failed to start: ${message}`,
    );
    diagnostic("languageClient.startFailed");
  }
}

async function resolveWorkbenchLoadedAddonInventory(
  context: vscode.ExtensionContext,
  serverPath: string,
  outputChannel: vscode.LogOutputChannel,
): Promise<string | undefined> {
  const configuration = vscode.workspace.getConfiguration(
    workbenchConfig.section,
  );
  const enabled = configuration.get(
    workbenchConfig.settings.enabled,
    workbenchDefaults.enabled,
  );
  const host = configuration.get(
    workbenchConfig.settings.host,
    workbenchDefaults.host,
  );
  const port = configuration.get(
    workbenchConfig.settings.port,
    workbenchDefaults.port,
  );
  const isCurrentConfiguration = (): boolean => {
    const current = vscode.workspace.getConfiguration(workbenchConfig.section);
    return current.get(
      workbenchConfig.settings.enabled,
      workbenchDefaults.enabled,
    ) === enabled
      && current.get(
        workbenchConfig.settings.host,
        workbenchDefaults.host,
      ) === host
      && current.get(
        workbenchConfig.settings.port,
        workbenchDefaults.port,
      ) === port;
  };
  const gateway = new WorkbenchGateway({
    enabled,
    endpoint: {
      host,
      port,
    },
    serverPath: Promise.resolve(serverPath),
    record: (record) => {
      diagnostic("workbenchGatewayDiagnosticRecord", {
        capability: record.capability,
        outcome: record.outcome,
        durationMs: record.durationMs,
        timing: record.timing ? JSON.stringify(record.timing) : undefined,
      });
    },
    onNetApiFailure: diagnosis => {
      if (isCurrentConfiguration()) {
        updateWorkbenchFailureNotification(diagnosis);
      }
    },
  });
  const result = await gateway.getLoadedAddonGraph();
  if (!isCurrentConfiguration()) {
    return undefined;
  }
  if (!result.ok) {
    outputChannel.appendLine(
      `Workbench-loaded add-on graph unavailable (${result.failure.category}): ${result.failure.recoveryHint}`,
    );
    logLanguageClientStartupTiming(
      context,
      "workbenchLoadedAddonGraphUnavailable",
      {
        category: result.failure.category,
      },
    );
    return undefined;
  }
  resetWorkbenchFailureNotification();
  let inventory;
  try {
    inventory = await writeLoadedAddonSourceInventory(context, result.value);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    outputChannel.appendLine(
      `Workbench-loaded add-on graph unresolved: ${message}`,
    );
    logLanguageClientStartupTiming(
      context,
      "workbenchLoadedAddonGraphUnresolved",
      {
        addons: result.value.addons.length,
      },
    );
    return undefined;
  }
  outputChannel.appendLine(
    `Workbench-loaded add-ons: ${result.value.addons.length} (bridge ${result.value.bridgeVersion})`,
  );
  logLanguageClientStartupTiming(context, "workbenchLoadedAddonGraphReady", {
    addons: result.value.addons.length,
    protocolVersion: result.value.protocolVersion,
    inventoryBytes: inventory.bytes,
    inventorySerializeAndHashMs: inventory.timingsMs.serializeAndHash,
    inventoryPublishMs: inventory.timingsMs.publish,
    inventoryTotalMs: inventory.timingsMs.total,
  });
  return inventory.path;
}

async function resolveCurrentWorkbenchAddonInventory(
  context: vscode.ExtensionContext,
  serverPath: string,
  outputChannel: vscode.LogOutputChannel,
  mode: ExternalIndexMode,
  workbenchReady?: Promise<boolean>,
): Promise<string | undefined> {
  if (mode === 'all' || mode === 'none' || !workbenchReady || !(await workbenchReady)) {
    return undefined;
  }
  return resolveWorkbenchLoadedAddonInventory(
    context,
    serverPath,
    outputChannel,
  );
}

function readExternalIndexMode(): ExternalIndexMode {
  const value = vscode.workspace.getConfiguration(workbenchConfig.section).get(
    workbenchConfig.settings.externalIndexMode,
    workbenchDefaults.externalIndexMode,
  );
  return typeof value === 'string' && externalIndexModes.includes(value as ExternalIndexMode)
    ? value as ExternalIndexMode
    : workbenchDefaults.externalIndexMode;
}

function logFirstSemanticTokenResponse(
  context: vscode.ExtensionContext,
  document: vscode.TextDocument,
  startedAt: number,
  status: string,
  error?: unknown,
): void {
  if (firstSemanticTokenTimingLogged) {
    return;
  }
  firstSemanticTokenTimingLogged = true;
  logLanguageClientStartupTiming(context, "firstSemanticTokenResponse", {
    status,
    uri: document.uri.toString(),
    languageId: document.languageId,
    elapsedMsForRequest: Date.now() - startedAt,
    message: error instanceof Error ? error.message : undefined,
  });
}

function createLanguageServerErrorHandler(): ErrorHandler {
  const restarts: number[] = [];
  return {
    error: (_error, _message, count) => {
      if (count !== undefined && count <= 3) {
        return { action: ErrorAction.Continue };
      }
      return { action: ErrorAction.Shutdown };
    },
    closed: () => {
      restarts.push(Date.now());
      if (restarts.length <= languageClientCrashHandling.maxRestartCount) {
        return { action: CloseAction.Restart };
      }

      const elapsed = restarts[restarts.length - 1] - restarts[0];
      if (elapsed <= languageClientCrashHandling.restartWindowMs) {
        void vscode.window.showErrorMessage(
          languageClientCrashHandling.finalCrashMessage,
        );
        return {
          action: CloseAction.DoNotRestart,
          message: languageClientCrashHandling.finalCrashMessage,
          handled: true,
        };
      }

      restarts.shift();
      return { action: CloseAction.Restart };
    },
  };
}

async function restartLanguageClient(
  context: vscode.ExtensionContext,
  outputChannel: vscode.LogOutputChannel,
  reason: string,
  waitForInitialStartup = true,
  workbenchReady?: Promise<boolean>,
): Promise<void> {
  await restartCoordinator.run(async () => {
    if (waitForInitialStartup) {
      const startup = initialStartup;
      if (startup) {
        await startup;
      }
    }
    outputChannel.appendLine(`Restarting language server: ${reason}`);
    languageClientStartGeneration += 1;
    disposeClientDisposables();
    const activeClient = client;
    client = undefined;
    try {
      if (activeClient) {
        await activeClient.stop();
      }
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      outputChannel.appendLine(
        `Language server stop during restart reported: ${message}`,
      );
    }
    const bracketColoring = getBracketColoringMode();
    await synchronizeBracketColoringEditorMode(bracketColoring, outputChannel);
    await startLanguageClient(
      context,
      outputChannel,
      bracketColoring,
      activeExternalIndexProgressSession?.progress,
      workbenchReady,
    );
  });
}

export function monitorExternalIndexProgress(
  context: vscode.ExtensionContext,
  activeClient: LanguageClient,
  progress: ExternalIndexProgress | undefined,
): {
  completion: Promise<void>;
  disposable: vscode.Disposable;
  waitForNextCompletion(progress?: ExternalIndexProgress, expectedPhase?: string): Promise<void>;
} {
  let complete = false;
  let initialIndexComplete = false;
  const pendingCompletions: Array<{
    progress?: ExternalIndexProgress;
    expectedPhase?: string;
    expectedPhaseObserved: boolean;
    resolve: () => void;
  }> = [];
  let resolveCompletion: (() => void) | undefined;
  const completion = new Promise<void>((resolve) => {
    resolveCompletion = resolve;
  });
  const resolvePendingCompletions = (force = false) => {
    for (let index = pendingCompletions.length - 1; index >= 0; index -= 1) {
      const pending = pendingCompletions[index];
      if (!force && !pending.expectedPhaseObserved) {
        continue;
      }
      pendingCompletions.splice(index, 1);
      pending.resolve();
    }
  };
  const finish = () => {
    if (!complete) {
      complete = true;
      resolveCompletion?.();
    }
    resolvePendingCompletions(true);
  };
  const reportProgress = (
    target: ExternalIndexProgress | undefined,
    params: ExternalIndexProgressParams,
  ) => {
    const isReady = params.phase === "complete" && params.status === "ready";
    if (!isReady) {
      target?.report({
        message: externalIndexProgressMessage(params.phase, params.status),
      });
    }
  };
  const finishIndex = (params: ExternalIndexProgressParams) => {
    logLanguageClientStartupTiming(context, "externalIndexReady", {
      status: params.status,
      gameDataFiles: params.gameDataFiles,
    });
    finish();
  };
  const notification = activeClient.onNotification(
    new NotificationType<ExternalIndexProgressParams>(
      languageClientNotifications.externalIndexProgress,
    ),
    (params) => {
      if (!initialIndexComplete) {
        reportProgress(progress, params);
      }
      for (const pending of pendingCompletions) {
        if (params.phase === pending.expectedPhase) {
          pending.expectedPhaseObserved = true;
        }
        reportProgress(pending.progress, params);
      }
      logLanguageClientStartupTiming(context, "externalIndexProgress", {
        phase: params.phase,
        status: params.status,
        gameDataFiles: params.gameDataFiles,
      });
      if (params.phase === "complete") {
        if (externalIndexProgressIsTerminal(params.status)) {
          initialIndexComplete = true;
          if (!complete) {
            finishIndex(params);
          }
          resolvePendingCompletions();
        }
      }
    },
  );
  const stateChanges = activeClient.onDidChangeState((event) => {
    if (event.newState === State.Stopped) {
      finish();
    }
  });
  return {
    completion,
    disposable: vscode.Disposable.from(notification, stateChanges, {
      dispose: finish,
    }),
    waitForNextCompletion: (nextProgress, expectedPhase) => new Promise((resolve) => {
      pendingCompletions.push({
        progress: nextProgress,
        expectedPhase,
        expectedPhaseObserved: expectedPhase === undefined,
        resolve,
      });
    }),
  };
}

export function externalIndexProgressIsTerminal(status?: string): boolean {
  return status !== "updating";
}

export function externalIndexProgressMessage(
  phase: string,
  status?: string,
): string {
  switch (phase) {
    case "inventory-load-start":
    case "inventory-load-end":
      return "Loading installed add-on inventory";
    case "addon-manifest-validate-start":
    case "addon-manifest-validate-end":
      return "Validating add-on identities";
    case "pac-inspect-start":
    case "pac-inspect-end":
      return "Inspecting installed add-on packs";
    case "addon-cache-loaded":
      return "Loaded unchanged add-on index";
    case "addon-rebuild-end":
      return "Rebuilt changed add-on index";
    case "offline":
      return "Loaded offline add-on indexes";
    case "workbench-reconciliation":
      return "Reconciled Workbench add-on indexes";
    case "addon-cache-failed":
      return "Add-on indexing failed";
    case "workspace-rebuild-start":
    case "workspace-rebuild-end":
      return "Indexing workspace scripts";
    case "complete":
      if (status === "ready") {
        return "Script index ready";
      }
      if (status === "failed") {
        return "Script indexing failed";
      }
      return "Script index unavailable";
    default:
      return "Indexing scripts";
  }
}

async function synchronizeBracketColoringEditorMode(
  mode: BracketColoringMode,
  outputChannel: vscode.LogOutputChannel,
): Promise<void> {
  try {
    await applyBracketColoringEditorMode(mode);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    outputChannel.appendLine(
      `Could not apply ${mode} bracket presentation: ${message}`,
    );
    diagnostic("languageClient.bracketColoringConfigurationFailed", {
      mode,
      message,
    });
  }
}

function disposeClientDisposables(): void {
  refreshWorkbenchGraph = undefined;
  for (const disposable of clientDisposables) {
    disposable.dispose();
  }
  clientDisposables = [];
}

function extensionModeName(mode: vscode.ExtensionMode): string {
  switch (mode) {
    case vscode.ExtensionMode.Development:
      return "development";
    case vscode.ExtensionMode.Production:
      return "production";
    case vscode.ExtensionMode.Test:
      return "test";
    default:
      return "unknown";
  }
}
