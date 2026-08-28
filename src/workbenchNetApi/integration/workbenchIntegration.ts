import * as vscode from 'vscode';
import { diagnostic } from '../../diagnostics/diagnostics';
import { workbenchConfig, workbenchDefaults } from '../../extensionConfig/workbench';
import {
	invokeWorkbenchPrivateApi,
	WorkbenchEndpoint,
	WorkbenchGatewayResult,
	WorkbenchIntegrationBootstrap,
	WorkbenchProcessStatus,
} from '../gateway/workbenchGateway';

// V3 gates all feature and index startup on an explicit response to the
// current consent prompt instead of inheriting the earlier late-startup gate.
const approvalStateKey = 'workbenchIntegrationApprovedV3';
const installChoice = 'Enable Workbench Integration';
// Where Workbench runs inside a Wine prefix, its options live in a registry
// that only accepts a write while no wineserver holds the prefix.
const installFailureMessage =
	'Reforger Workbench integration could not be installed. If Workbench is open, close it and reload the window.';
const restartMessage = 'Reforger Workbench integration was updated. Restart Workbench to activate it.';

export function workbenchStartupPolicy(
	enabled: boolean,
	hasExplicitValue: boolean,
): { enabled: boolean; promptWhenDisabled: boolean } {
	return {
		enabled,
		promptWhenDisabled: !hasExplicitValue || enabled,
	};
}

export interface WorkbenchIntegrationStatus {
	installed: boolean;
	installationAvailable: boolean;
	maintenanceRequired: boolean;
	profileAvailable: boolean;
	workbenchRunning: boolean;
	enfusionProtocolRegistered: boolean;
}

export interface WorkbenchIntegrationRuntime {
	status(
		endpoint: WorkbenchEndpoint,
	): Promise<WorkbenchGatewayResult<WorkbenchIntegrationStatus>>;
	bootstrap(
		endpoint: WorkbenchEndpoint,
	): Promise<WorkbenchGatewayResult<WorkbenchIntegrationBootstrap>>;
	maintain(
		endpoint: WorkbenchEndpoint,
	): Promise<WorkbenchGatewayResult<WorkbenchIntegrationBootstrap>>;
	processStatus(
		endpoint: WorkbenchEndpoint,
	): Promise<WorkbenchGatewayResult<WorkbenchProcessStatus>>;
}

export interface WorkbenchIntegrationUi {
	confirmInstall(): Promise<boolean>;
	runInstall<T>(task: () => Promise<T>): Promise<T>;
	showInstallFailed(message: string): void;
	showStartOrRestart?(running: boolean): void;
}

export interface WorkbenchIntegrationState {
	isApproved(): boolean;
	approve(): Promise<void>;
}

export class WorkbenchIntegrationCoordinator implements vscode.Disposable {
	private readonly ready: Promise<boolean>;
	private resolveReady: ((ready: boolean) => void) | undefined;
	private readonly consentSettled: Promise<boolean>;
	private resolveConsentSettled: ((approved: boolean) => void) | undefined;
	private startup: Promise<boolean> | undefined;
	private startupResult: boolean | undefined;
	private startupInProgress = false;
	private disableRequestedDuringStartup = false;
	private enabled: boolean;
	private connected = false;
	private disconnectedAfterRequiredRestart = false;
	private requiresRestart = false;
	private bootstrapFinished = false;
	private bootstrapInProgress = false;
	private profileWasMissing = false;
	private endpoint: WorkbenchEndpoint | undefined;
	private disposed = false;

	public constructor(
		private readonly state: WorkbenchIntegrationState,
		private readonly runtime: WorkbenchIntegrationRuntime,
		private readonly ui: WorkbenchIntegrationUi,
		enabled: boolean,
		private readonly enableWorkbench?: () => Promise<void>,
		private readonly promptWhenDisabled = true,
		private readonly disableWorkbench?: () => Promise<void>,
	) {
		this.enabled = enabled;
		this.ready = new Promise(resolve => {
			this.resolveReady = resolve;
		});
		this.consentSettled = new Promise(resolve => {
			this.resolveConsentSettled = resolve;
		});
	}

	public start(): Promise<boolean> {
		if (this.state.isApproved()) {
			this.resolveConsentSettled?.(true);
		}
		if (this.startup && (this.startupInProgress
			|| this.startupResult !== false
			|| !this.enabled)) {
			return this.ready;
		}
		if (this.startupResult === false && !this.enabled) {
			return this.ready;
		}
		if (!this.enabled && (!this.promptWhenDisabled || this.state.isApproved())) {
			this.resolveConsentSettled?.(this.state.isApproved());
			this.resolveReady?.(true);
			return this.ready;
		}
		this.startupInProgress = true;
		this.startup = this.bootstrapStartup()
			.then(result => {
				this.startupInProgress = false;
				this.startupResult = result;
				return result;
			})
			.catch(error => {
				this.resolveConsentSettled?.(false);
				this.startupInProgress = false;
				const message = error instanceof Error ? error.message : String(error);
				diagnostic('workbenchIntegrationStartupFailed', { message });
				this.ui.showInstallFailed(installFailureMessage);
				this.bootstrapFinished = true;
				this.startupResult = false;
				this.resolveReady?.(false);
				return false;
			});
		return this.ready;
	}

	public onWorkbenchConfigurationChanged(
		enabled: boolean,
		explicitlyDisabled = false,
	): void {
		if (this.disposed) {
			return;
		}
		this.enabled = enabled;
		if (enabled) {
			this.disableRequestedDuringStartup = false;
			const currentStartup = this.startup;
			if (this.startupInProgress && currentStartup) {
				void currentStartup.then(() => {
					if (this.startup === currentStartup
						&& this.enabled
						&& this.startupResult === false) {
						void this.start();
					}
				});
			} else {
				void this.start();
			}
		} else {
			if (explicitlyDisabled && !this.state.isApproved()) {
				this.disableRequestedDuringStartup = true;
			}
			this.onWorkbenchDisconnected();
		}
	}

	public async onWorkbenchConnected(endpoint: WorkbenchEndpoint): Promise<void> {
		if (this.disposed || !this.enabled) {
			return;
		}
		this.endpoint = endpoint;
		this.connected = true;
		if (this.bootstrapInProgress) {
			return;
		}
		if (this.profileWasMissing) {
			await this.retryMissingProfile();
		}
		this.finishIfReady();
	}

	public onWorkbenchDisconnected(): void {
		this.connected = false;
		if (this.requiresRestart) {
			this.disconnectedAfterRequiredRestart = true;
		}
	}

	public whenReady(): Promise<boolean> {
		return this.ready;
	}

	public whenConsentSettled(): Promise<boolean> {
		return this.consentSettled;
	}

	public dispose(): void {
		this.disposed = true;
		this.resolveConsentSettled?.(false);
		this.resolveReady?.(false);
	}

	private async bootstrapStartup(): Promise<boolean> {
		const endpoint = this.readEndpoint();
		const status = await this.runtime.status(endpoint);
		if (!status.ok) {
			diagnostic('workbenchIntegrationStatusUnavailable', {
				category: status.failure.category,
			});
		}
		const approved = this.state.isApproved();

		let result: WorkbenchGatewayResult<WorkbenchIntegrationBootstrap>;
		if (!approved) {
			if (!(await this.ui.confirmInstall())) {
				await this.disableWorkbench?.();
				this.resolveConsentSettled?.(false);
				this.bootstrapFinished = true;
				this.resolveReady?.(false);
				return false;
			}
			this.resolveConsentSettled?.(true);
			result = await this.ui.runInstall(() => this.runtime.bootstrap(endpoint));
		} else if (status.ok
			&& status.value.installed
			&& status.value.profileAvailable
			&& !status.value.maintenanceRequired
			&& status.value.enfusionProtocolRegistered) {
			result = { ok: true, value: {
				netApiEnabled: true,
				netApiWritePerformed: false,
				enfusionProtocolRegistered: true,
				enfusionProtocolWritePerformed: false,
				bridgeInstalled: true,
				bridgeChanged: false,
				profileAvailable: true,
			} };
		} else if (status.ok && !status.value.enfusionProtocolRegistered) {
			result = await this.ui.runInstall(() => this.runtime.bootstrap(endpoint));
		} else {
			result = await this.ui.runInstall(() => this.runtime.maintain(endpoint));
		}
		if (this.disposed || (approved && !this.enabled)) {
			return false;
		}
		if (!result.ok) {
			this.ui.showInstallFailed(installFailureMessage);
			this.bootstrapFinished = true;
			this.resolveReady?.(false);
			return false;
		}
		if (!approved) {
			if (this.disableRequestedDuringStartup) {
				this.bootstrapFinished = true;
				this.resolveReady?.(false);
				return false;
			}
			if (this.enableWorkbench) {
				await this.enableWorkbench();
			}
			this.enabled = true;
			await this.state.approve();
		}

		this.bootstrapFinished = true;
		this.profileWasMissing = !result.value.profileAvailable;
		if (result.value.bridgeChanged) {
			if (!this.enabled) {
				return false;
			}
			await this.handleBridgeChange(endpoint);
			return true;
		}
		if (!status.ok || !status.value.workbenchRunning) {
			this.resolveReady?.(true);
			return true;
		}
		this.finishIfReady();
		return true;
	}

	private async retryMissingProfile(): Promise<void> {
		if (!this.endpoint || !this.profileWasMissing || this.bootstrapInProgress) {
			return;
		}
		this.bootstrapInProgress = true;
		try {
			const result = await this.ui.runInstall(() => this.runtime.maintain(this.endpoint!));
			if (!this.enabled) {
				return;
			}
			if (!result.ok) {
				this.ui.showInstallFailed(installFailureMessage);
				this.resolveReady?.(false);
				return;
			}
			this.profileWasMissing = !result.value.profileAvailable;
			if (result.value.bridgeChanged) {
				await this.handleBridgeChange(this.endpoint);
			}
		} finally {
			this.bootstrapInProgress = false;
		}
	}

	private async handleBridgeChange(endpoint: WorkbenchEndpoint): Promise<void> {
		if (this.disposed || !this.enabled) {
			return;
		}
		this.requiresRestart = true;
		this.disconnectedAfterRequiredRestart = false;
		const process = await this.runtime.processStatus(endpoint);
		if (process.ok && process.value.isOpen) {
			this.ui.showStartOrRestart?.(true);
			return;
		}
		this.requiresRestart = false;
		this.resolveReady?.(true);
	}

	private finishIfReady(): void {
		if (this.bootstrapFinished
			&& this.connected
			&& (!this.requiresRestart || this.disconnectedAfterRequiredRestart)
			&& !this.profileWasMissing) {
			this.resolveReady?.(true);
		}
	}

	private readEndpoint(): WorkbenchEndpoint {
		const configuration = vscode.workspace.getConfiguration('reforgerScriptTools.workbench');
		return {
			host: configuration.get('host', '127.0.0.1'),
			port: configuration.get('port', 5775),
		};
	}
}

export function createWorkbenchIntegration(
	context: vscode.ExtensionContext,
	serverPath: Promise<string | undefined>,
): WorkbenchIntegrationCoordinator {
	const configuration = vscode.workspace.getConfiguration('reforgerScriptTools.workbench');
	const invoke = (
		workbenchEndpoint: WorkbenchEndpoint,
		action: Parameters<typeof invokeWorkbenchPrivateApi>[2],
		deadline: number,
	) => invokeWorkbenchPrivateApi(serverPath, workbenchEndpoint, action, deadline);
	const runtime: WorkbenchIntegrationRuntime = {
		status: async workbenchEndpoint => decodeStatus(await invoke(workbenchEndpoint, 'integration-status', 1_500)),
		bootstrap: async workbenchEndpoint => decodeBootstrap(await invoke(workbenchEndpoint, 'bootstrap-integration', 120_000)),
		maintain: async workbenchEndpoint => decodeBootstrap(await invoke(workbenchEndpoint, 'maintain-integration', 120_000)),
		processStatus: async workbenchEndpoint => decodeProcessStatus(await invoke(workbenchEndpoint, 'process-status', 5_000)),
	};
	const ui: WorkbenchIntegrationUi = {
		confirmInstall: async () => (await vscode.window.showInformationMessage(
				'Enable Reforger Workbench Integration? This enables the Workbench setting, local integration API, and managed bridge installer.',
				installChoice,
		)) === installChoice,
		runInstall: task => Promise.resolve(vscode.window.withProgress(
			{
				location: vscode.ProgressLocation.Notification,
				title: 'Setting up Reforger Workbench Integration…',
				cancellable: false,
			},
			task,
		)),
		showInstallFailed: message => void vscode.window.showWarningMessage(message),
		showStartOrRestart: running => {
			if (running) {
				void vscode.window.showInformationMessage(restartMessage);
			}
		},
	};
	const enabled = configuration.get(
		workbenchConfig.settings.enabled,
		workbenchDefaults.enabled,
	);
	const enablementScope = configurationEnablementScope(configuration);
	const startup = workbenchStartupPolicy(
		enabled,
		enablementScope.hasExplicitValue,
	);
	return new WorkbenchIntegrationCoordinator(
		{
			isApproved: () => context.globalState.get<boolean>(approvalStateKey, false),
			approve: () => Promise.resolve(context.globalState.update(approvalStateKey, true)),
		},
		runtime,
		ui,
		startup.enabled,
		async () => {
			await configuration.update(
				workbenchConfig.settings.enabled,
				true,
				configurationEnablementScope(configuration).target,
			);
		},
		startup.promptWhenDisabled,
		async () => {
			await configuration.update(
				workbenchConfig.settings.enabled,
				false,
				configurationEnablementScope(configuration).target,
			);
		},
	);
}

function configurationEnablementScope(
	configuration: vscode.WorkspaceConfiguration,
): { hasExplicitValue: boolean; target: vscode.ConfigurationTarget } {
	const inspected = configuration.inspect<boolean>(workbenchConfig.settings.enabled);
	if (inspected?.workspaceFolderValue !== undefined
		|| inspected?.workspaceFolderLanguageValue !== undefined) {
		return { hasExplicitValue: true, target: vscode.ConfigurationTarget.WorkspaceFolder };
	}
	if (inspected?.workspaceValue !== undefined
		|| inspected?.workspaceLanguageValue !== undefined) {
		return { hasExplicitValue: true, target: vscode.ConfigurationTarget.Workspace };
	}
	if (inspected?.globalValue !== undefined || inspected?.globalLanguageValue !== undefined) {
		return { hasExplicitValue: true, target: vscode.ConfigurationTarget.Global };
	}
	return { hasExplicitValue: false, target: vscode.ConfigurationTarget.Global };
}

function decodeStatus(
	result: WorkbenchGatewayResult<unknown>,
): WorkbenchGatewayResult<WorkbenchIntegrationStatus> {
	if (!result.ok) {
		return result;
	}
	const value = result.value;
	if (!isRecord(value)
		|| !isRecord(value.bridge)
		|| typeof value.bridge.installed !== 'boolean'
		|| typeof value.bridge.installationAvailable !== 'boolean'
		|| typeof value.bridge.maintenanceRequired !== 'boolean'
		|| !isRecord(value.profile)
		|| typeof value.profile.exists !== 'boolean'
		|| typeof value.enfusionProtocolRegistered !== 'boolean') {
		return protocolFailure();
	}
	const native = isRecord(value.native) && typeof value.native.isRunning === 'boolean'
		? value.native.isRunning
		: false;
	return { ok: true, value: {
		installed: value.bridge.installed,
		installationAvailable: value.bridge.installationAvailable,
		maintenanceRequired: value.bridge.maintenanceRequired,
		profileAvailable: value.profile.exists,
		workbenchRunning: native,
		enfusionProtocolRegistered: value.enfusionProtocolRegistered,
	} };
}

function decodeBootstrap(
	result: WorkbenchGatewayResult<unknown>,
): WorkbenchGatewayResult<WorkbenchIntegrationBootstrap> {
	if (!result.ok) {
		return result;
	}
	const value = result.value;
	if (!isRecord(value)
		|| typeof value.netApiEnabled !== 'boolean'
		|| typeof value.netApiWritePerformed !== 'boolean'
		|| typeof value.enfusionProtocolRegistered !== 'boolean'
		|| typeof value.enfusionProtocolWritePerformed !== 'boolean'
		|| typeof value.bridgeInstalled !== 'boolean'
		|| (value.bridgeVersion !== undefined && typeof value.bridgeVersion !== 'string')
		|| typeof value.bridgeChanged !== 'boolean'
		|| typeof value.profileAvailable !== 'boolean') {
		return protocolFailure();
	}
	return { ok: true, value: {
		netApiEnabled: value.netApiEnabled,
		netApiWritePerformed: value.netApiWritePerformed,
		enfusionProtocolRegistered: value.enfusionProtocolRegistered,
		enfusionProtocolWritePerformed: value.enfusionProtocolWritePerformed,
		bridgeInstalled: value.bridgeInstalled,
		...(value.bridgeVersion === undefined ? {} : { bridgeVersion: value.bridgeVersion }),
		bridgeChanged: value.bridgeChanged,
		profileAvailable: value.profileAvailable,
	} };
}

function decodeProcessStatus(
	result: WorkbenchGatewayResult<unknown>,
): WorkbenchGatewayResult<WorkbenchProcessStatus> {
	if (!result.ok) {
		return result;
	}
	const value = result.value;
	if (!isRecord(value)
		|| typeof value.isOpen !== 'boolean'
		|| (value.processId !== undefined && typeof value.processId !== 'number')
		|| (value.projectPath !== undefined && typeof value.projectPath !== 'string')) {
		return protocolFailure();
	}
	return { ok: true, value: {
		isOpen: value.isOpen,
		...(value.processId === undefined ? {} : { processId: value.processId }),
		...(value.projectPath === undefined ? {} : { projectPath: value.projectPath }),
	} };
}

function protocolFailure(): WorkbenchGatewayResult<never> {
	return { ok: false, failure: {
		category: 'protocol',
		recoveryHint: 'Workbench integration returned an invalid response.',
	} };
}

function isRecord(value: unknown): value is Record<string, unknown> {
	return typeof value === 'object' && value !== null && !Array.isArray(value);
}
