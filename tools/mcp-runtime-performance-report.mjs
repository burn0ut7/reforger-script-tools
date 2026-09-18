#!/usr/bin/env node

import { createHash } from 'node:crypto';
import { execFileSync, spawn } from 'node:child_process';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { arch, cpus, hostname, platform, release, totalmem } from 'node:os';
import { dirname, resolve } from 'node:path';
import { createInterface } from 'node:readline';
import { performance } from 'node:perf_hooks';

const DEFAULT_JSON_OUT = 'tools/reports/mcp-runtime-performance.report.json';
const DEFAULT_MARKDOWN_OUT = 'tools/reports/mcp-runtime-performance.report.md';
const GAME_DATA_INITIALIZATION_BUDGET_MS = 120_000;
const SEMANTIC_BUDGET_MS = 5_000;
const TEXT_SEARCH_BUDGET_MS = 30_000;
const WIKI_BUDGET_MS = 5_000;
const RELATIONSHIP_FIRST_BUDGET_MS = 1_000;
const RELATIONSHIP_ONE_LEVEL_MEDIAN_BUDGET_MS = 100;
const RELATIONSHIP_BROAD_P95_BUDGET_MS = 500;

async function runReport(configuration) {
	const client = new McpClient(
		resolve(configuration.server),
		serverArguments(configuration),
		configuration.timeoutMs,
	);
	const operations = [];
	let listedToolCount = 0;
	let initialize;
	let toolsList;
	let processToInitializeMs = 0;
	let concurrency = { operation: undefined, probes: [] };
	const memory = [];
	let corpus = {};
	try {
		initialize = await measure(() => client.initialize());
		captureWorkingSet(client, memory, 'after-initialize');
		processToInitializeMs = client.processElapsedMs();
		const listed = await measure(() => client.listTools());
		toolsList = {
			elapsedMs: round(listed.elapsedMs),
			responseBytes: responseBytes(listed.value),
		};
		const toolNames = new Set(
			(listed.value.tools ?? [])
				.map(tool => tool.name)
				.filter(name => typeof name === 'string' && !name.startsWith('workbench_')),
		);
		listedToolCount = toolNames.size;
		const runner = new ScenarioRunner(client, toolNames, operations, configuration, memory);
		await runScenarios(runner);
		corpus = runner.corpus;
		for (const name of toolNames) {
			if (!runner.seen.has(name)) {
				runner.skip(name, 'No report scenario exists for this newly listed non-Workbench tool.');
			}
		}
		concurrency = await runner.runConcurrencyProbe();
	} catch (error) {
		operations.push({
			name: error.phase ?? 'runner',
			family: 'Protocol',
			status: 'runner-error',
			reason: sanitizeMessage(`${error.message}${client.stderrTail ? `; stderr: ${client.stderrTail}` : ''}`),
			firstMs: 0,
			warm: emptyDistribution(),
			responseBytes: 0,
		});
	} finally {
		await client.close();
	}

	const coldProcess = await measureColdProcesses(configuration);
	const exercised = operations.filter(operation => !operation.synthetic && !['skipped', 'missing'].includes(operation.status)).length;
	const skipped = operations.filter(operation => !operation.synthetic && ['skipped', 'missing'].includes(operation.status)).length;
	const correctnessFailures = operations.filter(operation => ['tool-error', 'runner-error', 'unstable-result'].includes(operation.status)).length;
	const overBudget = operations.reduce(
		(total, operation) => total
			+ (operation.status === 'over-budget' ? 1 : 0)
			+ (operation.variants ?? []).filter(variant => variant.status === 'over-budget').length,
		0,
	);
	const concurrencyFailures = concurrency.probes.reduce((total, probe) => total + probe.failed, 0);
	const relationshipGates = evaluateRelationshipGates(operations);
	const failed = correctnessFailures + concurrencyFailures + coldProcess.failed
		+ (configuration.enforceBudgets ? overBudget + relationshipGates.filter(gate => gate.status === 'fail').length : 0);
	const verdict = failed > 0 ? 'fail' : skipped > 0 ? 'partial' : 'pass';
	return {
		schemaVersion: 2,
		generatedAt: new Date().toISOString(),
		scope: 'non-workbench',
		server: resolve(configuration.server),
		configuration: {
			toolProfile: 'all',
			commit: configuration.commit,
			buildProfile: configuration.buildProfile,
			acceptProjectionMemory: configuration.acceptProjectionMemory,
			samples: configuration.samples,
			concurrencyLevels: configuration.concurrencyLevels,
			timeoutMs: configuration.timeoutMs,
			requireAll: configuration.requireAll,
			enforceBudgets: configuration.enforceBudgets,
			coldSamples: configuration.coldSamples,
			workspaceRootCount: configuration.workspaceScripts.length,
			workspaceRoots: configuration.workspaceScripts.map(root => resolve(root)),
			workspaceIndexInventory: configuration.workspaceIndexInventory
				? {
					path: resolve(configuration.workspaceIndexInventory.path),
					sha256: configuration.workspaceIndexInventory.sha256,
					indexedSymbols: configuration.workspaceIndexInventory.indexedSymbols,
				}
				: undefined,
			dependencyProjectCount: configuration.dependencyProjects.length,
			dependencyProjects: configuration.dependencyProjects.map(project => resolve(project)),
			addonSourceInventory: configuration.addonSourceInventory ? resolve(configuration.addonSourceInventory) : undefined,
			addonIndexStorage: configuration.addonIndexStorage ? resolve(configuration.addonIndexStorage) : undefined,
			officialWikiRoot: configuration.officialWikiRoot ? resolve(configuration.officialWikiRoot) : undefined,
			externalIndexMode: configuration.externalIndexMode,
			queries: {
				gameSymbol: configuration.gameSymbolQuery,
				workspaceSymbol: configuration.workspaceSymbolQuery,
				text: configuration.textQuery,
				broadText: configuration.broadTextQuery,
				regex: configuration.regexQuery,
				wiki: configuration.wikiQuery,
				relationshipMethod: configuration.relationshipMethodQuery,
			},
		},
		host: {
			hostname: hostname(),
			platform: platform(),
			release: release(),
			architecture: arch(),
			logicalCpuCount: cpus().length,
			totalMemoryBytes: totalmem(),
		},
		protocol: {
			initializeMs: round(initialize?.elapsedMs ?? 0),
			processToInitializeMs: round(processToInitializeMs),
			initializeResponseBytes: responseBytes(initialize?.value),
			toolsListMs: toolsList?.elapsedMs ?? 0,
			toolsListResponseBytes: toolsList?.responseBytes ?? 0,
		},
		coverage: { listed: listedToolCount, exercised, skipped, failed, overBudget },
		operations,
		corpus,
		memory,
		relationshipGates,
		concurrency,
		coldProcess,
		verdict,
	};
}

async function measurePairedRuns(configuration) {
	if (!configuration.pairedBaselineServer) return undefined;
	const cold = {
		baselineInitialize: [], candidateInitialize: [],
		baselineToolsList: [], candidateToolsList: [],
		baselineStatus: [], candidateStatus: [],
	};
	for (let pair = 0; pair < configuration.coldSamples; pair += 1) {
		const order = pair % 2 === 0 ? ['baseline', 'candidate'] : ['candidate', 'baseline'];
		for (const label of order) {
			const server = label === 'baseline' ? configuration.pairedBaselineServer : configuration.server;
			const client = new McpClient(resolve(server), serverArguments(configuration), configuration.timeoutMs);
			try {
				await client.initialize();
				cold[`${label}Initialize`].push(client.processElapsedMs());
				const toolsList = await measure(() => client.listTools());
				cold[`${label}ToolsList`].push(toolsList.elapsedMs);
				const status = await measure(() => client.callTool('game_data_status', {}));
				cold[`${label}Status`].push(status.elapsedMs);
			} finally {
				await client.close();
			}
		}
	}
	const clients = {
		baseline: new McpClient(resolve(configuration.pairedBaselineServer), serverArguments(configuration), configuration.timeoutMs),
		candidate: new McpClient(resolve(configuration.server), serverArguments(configuration), configuration.timeoutMs),
	};
	const contexts = {};
	const scenarios = [
		{ name: 'game_data_status', argumentsValue: {} },
		{ name: 'search_game_data_symbols', argumentsValue: { query: configuration.gameSymbolQuery, limit: 20 } },
		{ name: 'search_workspace_symbols', argumentsValue: { query: configuration.workspaceSymbolQuery, limit: 20 } },
		{
			name: 'current_mixed_source_client_search',
			invoke: async client => {
				const [gameData, workspace] = await Promise.all([
					client.callTool('search_game_data_symbols', { query: configuration.gameSymbolQuery, limit: 20 }),
					client.callTool('search_workspace_symbols', { query: configuration.workspaceSymbolQuery, limit: 20 }),
				]);
				return mergeSemanticSearchResponses(gameData, workspace);
			},
		},
		{ name: 'search_game_data_text', argumentsValue: { query: configuration.textQuery, limit: 20 } },
		{ name: 'search_workspace_text', argumentsValue: { query: configuration.textQuery, limit: 20 } },
		{ name: 'inspect_game_data_symbol', argumentsValue: label => ({ symbolRef: contexts[label].gameResult.symbolRef }) },
		{ name: 'list_game_data_symbol_members', argumentsValue: label => ({ symbolRef: contexts[label].gameResult.symbolRef, limit: 20 }) },
		{ name: 'read_game_data_source', argumentsValue: label => contexts[label].gameResult.readSourceInput },
		{ name: 'inspect_workspace_symbol', argumentsValue: label => ({ symbolRef: contexts[label].workspaceResult.symbolRef }) },
		{ name: 'list_workspace_symbol_members', argumentsValue: label => ({ symbolRef: contexts[label].workspaceResult.symbolRef, limit: 20 }) },
		{ name: 'query_workspace_symbol_relationships', argumentsValue: label => ({ symbolRef: contexts[label].workspaceResult.symbolRef, relationshipKinds: ['reference'], limit: 20 }) },
		{ name: 'read_workspace_source', argumentsValue: label => contexts[label].workspaceResult.readSourceInput },
		{ name: 'official_wiki_status', argumentsValue: {} },
	];
	const invokeScenario = (client, scenario, label) => scenario.invoke
		? scenario.invoke(client, label)
		: client.callTool(
			scenario.name,
			typeof scenario.argumentsValue === 'function' ? scenario.argumentsValue(label) : scenario.argumentsValue,
		);
	try {
		for (const [label, client] of Object.entries(clients)) {
			await client.initialize();
			await client.callTool('game_data_status', {});
			const gameSearch = summarizeResult(await client.callTool('search_game_data_symbols', { query: configuration.gameSymbolQuery, limit: 20 })).structured;
			const workspaceSearch = summarizeResult(await client.callTool('search_workspace_symbols', { query: configuration.workspaceSymbolQuery, limit: 20 })).structured;
			contexts[label] = {
				gameResult: firstResult(gameSearch),
				workspaceResult: firstResult(workspaceSearch),
			};
		}
		if (Object.values(contexts).every(context => context.gameResult?.symbolRef && context.gameResult?.readSourceInput
			&& context.workspaceResult?.symbolRef && context.workspaceResult?.readSourceInput)) {
			const records = new Map(scenarios.map(scenario => [scenario.name, { baseline: [], candidate: [], fingerprints: { baseline: [], candidate: [] } }]));
			for (const [label, client] of Object.entries(clients)) {
				for (const scenario of scenarios) await invokeScenario(client, scenario, label);
			}
		for (let sample = 0; sample < configuration.samples; sample += 1) {
			const order = sample % 2 === 0 ? ['baseline', 'candidate'] : ['candidate', 'baseline'];
			for (const scenario of scenarios) {
				for (const label of order) {
					const measured = await measure(() => invokeScenario(clients[label], scenario, label));
					const record = records.get(scenario.name);
					record[label].push(measured.elapsedMs);
					record.fingerprints[label].push(resultFingerprint(measured.value));
				}
			}
		}
			contexts.records = records;
		}
	} finally {
		await Promise.all(Object.values(clients).map(client => client.close()));
	}
	const coldDistributions = Object.fromEntries(Object.entries(cold).map(([name, values]) => [name, distribution(values)]));
	const coldPhases = {
		processToInitialize: pairedRegressionGate(coldDistributions.baselineInitialize, coldDistributions.candidateInitialize),
		toolsList: pairedRegressionGate(coldDistributions.baselineToolsList, coldDistributions.candidateToolsList),
		gameDataReadiness: pairedRegressionGate(coldDistributions.baselineStatus, coldDistributions.candidateStatus),
	};
	const records = contexts.records ?? new Map();
	return {
		method: 'Alternating baseline/candidate order per cold pair and warm sample; one uncounted warm-up per operation.',
		cold: coldDistributions,
		coldGate: {
			status: Object.values(coldPhases).every(gate => gate.status === 'pass') ? 'pass' : 'fail',
			phases: coldPhases,
		},
		operations: scenarios.filter(scenario => records.has(scenario.name)).map(scenario => {
			const record = records.get(scenario.name);
			const baseline = distribution(record.baseline);
			const candidate = distribution(record.candidate);
			const medianBudgetMs = Math.max(baseline.medianMs + 5, baseline.medianMs * 1.1);
			const p95BudgetMs = Math.max(baseline.p95Ms + 10, baseline.p95Ms * 1.2);
			const fingerprintsMatch = [...record.fingerprints.baseline, ...record.fingerprints.candidate]
				.every(value => value === record.fingerprints.baseline[0]);
			return {
				name: scenario.name,
				baseline,
				candidate,
				medianBudgetMs: round(medianBudgetMs),
				p95BudgetMs: round(p95BudgetMs),
				fingerprintsMatch,
				status: fingerprintsMatch && candidate.medianMs <= medianBudgetMs && candidate.p95Ms <= p95BudgetMs ? 'pass' : 'fail',
			};
		}),
	};
}

function pairedRegressionGate(baseline, candidate) {
	const medianBudgetMs = Math.max(baseline.medianMs + 5, baseline.medianMs * 1.1);
	const p95BudgetMs = Math.max(baseline.p95Ms + 10, baseline.p95Ms * 1.2);
	return {
		medianBudgetMs: round(medianBudgetMs),
		p95BudgetMs: round(p95BudgetMs),
		status: candidate.medianMs <= medianBudgetMs && candidate.p95Ms <= p95BudgetMs ? 'pass' : 'fail',
	};
}

function mergeSemanticSearchResponses(gameData, workspace) {
	const gameSummary = summarizeResult(gameData);
	const workspaceSummary = summarizeResult(workspace);
	const results = [
		...(gameSummary.structured?.results ?? []).map(result => ({ ...result, source: 'gameData' })),
		...(workspaceSummary.structured?.results ?? []).map(result => ({ ...result, source: 'workspace' })),
	];
	return {
		isError: gameSummary.isError || workspaceSummary.isError,
		structuredContent: {
			returned: results.length,
			total: (gameSummary.public.total ?? 0) + (workspaceSummary.public.total ?? 0),
			results,
		},
	};
}

class ScenarioRunner {
	constructor(client, toolNames, operations, configuration, memory) {
		this.client = client;
		this.toolNames = toolNames;
		this.operations = operations;
		this.configuration = configuration;
		this.seen = new Set();
		this.concurrencyCandidate = undefined;
		this.memory = memory;
		this.corpus = {};
	}

	async exercise(name, argumentsValue, settings = {}) {
		this.seen.add(name);
		if (!this.toolNames.has(name)) {
			return undefined;
		}
		const first = await measure(() => this.client.callTool(name, argumentsValue));
		if (settings.memoryAfterFirst) captureWorkingSet(this.client, this.memory, settings.memoryAfterFirst);
		const firstSummary = summarizeResult(first.value);
		if (firstSummary.isError && (settings.unavailableCodes ?? []).includes(firstSummary.code)) {
			this.operations.push({
				...skippedOperation(name, `Tool reported ${firstSummary.code ?? 'an unavailable source'}.`),
				firstMs: round(first.elapsedMs),
				responseBytes: responseBytes(first.value),
				code: firstSummary.code,
			});
			return undefined;
		}
		const warmSamples = [];
		const fingerprints = [resultFingerprint(first.value)];
		const responseSizes = [responseBytes(first.value)];
		let last = first.value;
		await this.client.callTool(name, argumentsValue);
		for (let iteration = 0; iteration < this.configuration.samples; iteration += 1) {
			const sample = await measure(() => this.client.callTool(name, argumentsValue));
			warmSamples.push(sample.elapsedMs);
			fingerprints.push(resultFingerprint(sample.value));
			responseSizes.push(responseBytes(sample.value));
			last = sample.value;
		}
		if (settings.memoryAfterWarm) captureWorkingSet(this.client, this.memory, settings.memoryAfterWarm);
		const summary = summarizeResult(last);
		const budgetMs = operationBudget(name);
		const stable = fingerprints.every(fingerprint => fingerprint === fingerprints[0]);
		const status = summary.isError
			? 'tool-error'
			: !stable
				? 'unstable-result'
			: Math.max(first.elapsedMs, ...warmSamples) > budgetMs
				? 'over-budget'
				: 'pass';
		const operation = {
			name,
			scenario: settings.scenario,
			family: toolFamily(name),
			status,
			firstMs: round(first.elapsedMs),
			warm: distribution(warmSamples),
			budgetMs,
			responseBytes: Math.max(...responseSizes),
			fingerprint: fingerprints.at(-1),
			stableFingerprint: stable,
			variants: [],
			reason: summary.isError ? toolErrorReason(summary.code) : undefined,
			...summary.public,
		};
		this.operations.push(operation);
		if (!summary.isError && (name === 'query_source_symbol_relationships'
			|| (!this.concurrencyCandidate && isConcurrencyCandidate(name)))) {
			this.concurrencyCandidate = { name, argumentsValue };
		}
		return firstSummary.structured;
	}

	async variant(name, argumentsValue, scenario) {
		const operation = this.operations.find(candidate => candidate.name === name);
		if (!operation || ['skipped', 'missing'].includes(operation.status)) return undefined;
		const samples = [];
		let sample;
		await this.client.callTool(name, argumentsValue);
		for (let iteration = 0; iteration < Math.max(1, this.configuration.samples); iteration += 1) {
			sample = await measure(() => this.client.callTool(name, argumentsValue));
			samples.push(sample.elapsedMs);
		}
		const summary = summarizeResult(sample.value);
		const budgetMs = operationBudget(name);
		const status = summary.isError ? 'tool-error' : sample.elapsedMs > budgetMs ? 'over-budget' : 'pass';
		operation.variants.push({
			scenario,
			status,
			elapsedMs: round(sample.elapsedMs),
			warm: distribution(samples),
			responseBytes: responseBytes(sample.value),
			fingerprint: resultFingerprint(sample.value),
			reason: summary.isError ? toolErrorReason(summary.code) : undefined,
			...summary.public,
		});
		if (status === 'tool-error') {
			operation.status = status;
		}
		return summary.structured;
	}

	async cancellation(name, argumentsValue) {
		const operation = this.operations.find(candidate => candidate.name === name);
		if (!operation || ['skipped', 'missing'].includes(operation.status)) return;
		const sample = await measure(() => this.client.cancelTool(name, argumentsValue));
		operation.variants.push({
			scenario: 'cancellation',
			status: 'pass',
			elapsedMs: round(sample.elapsedMs),
			warm: distribution([sample.elapsedMs]),
			responseBytes: 0,
			cancellationOutcome: sample.value,
		});
	}

	async mixedSemanticSearch(gameArguments, workspaceArguments) {
		const name = 'current_mixed_source_client_search';
		const invoke = async () => {
			const [gameData, workspace] = await Promise.all([
				this.client.callTool('search_game_data_symbols', gameArguments),
				this.client.callTool('search_workspace_symbols', workspaceArguments),
			]);
			return mergeSemanticSearchResponses(gameData, workspace);
		};
		const first = await measure(invoke);
		await invoke();
		const warmSamples = [];
		const fingerprints = [resultFingerprint(first.value)];
		let last = first.value;
		for (let iteration = 0; iteration < this.configuration.samples; iteration += 1) {
			const sample = await measure(invoke);
			warmSamples.push(sample.elapsedMs);
			fingerprints.push(resultFingerprint(sample.value));
			last = sample.value;
		}
		const summary = summarizeResult(last);
		const stable = fingerprints.every(fingerprint => fingerprint === fingerprints[0]);
		this.operations.push({
			name,
			synthetic: true,
			family: 'semantic-search',
			status: summary.isError ? 'tool-error' : stable ? 'pass' : 'unstable-result',
			firstMs: round(first.elapsedMs),
			warm: distribution(warmSamples),
			budgetMs: operationBudget('search_game_data_symbols'),
			responseBytes: responseBytes(last),
			fingerprint: fingerprints.at(-1),
			stableFingerprint: stable,
			variants: [],
			...summary.public,
		});
	}

	skip(name, reason) {
		if (this.seen.has(name)) return;
		this.seen.add(name);
		if (this.toolNames.has(name)) this.operations.push(skippedOperation(name, reason));
	}

	skipMany(names, reason) {
		for (const name of names) this.skip(name, reason);
	}

	async runConcurrencyProbe() {
		const candidate = this.concurrencyCandidate;
		if (!candidate || this.configuration.concurrencyLevels.length === 0) {
			return { operation: undefined, probes: [] };
		}
		const probes = [];
		for (const requested of this.configuration.concurrencyLevels) {
			const started = performance.now();
			const calls = Array.from({ length: requested }, async () => {
				const sample = await measure(() => this.client.callTool(candidate.name, candidate.argumentsValue));
				return { elapsedMs: sample.elapsedMs, ok: sample.value?.isError !== true };
			});
			const results = await Promise.all(calls);
			const completed = results.filter(result => result.ok).length;
			probes.push({
				requested,
				completed,
				failed: requested - completed,
				elapsedMs: round(performance.now() - started),
				individual: distribution(results.map(result => result.elapsedMs)),
			});
		}
		return {
			operation: candidate.name,
			probes,
		};
	}
}

async function runScenarios(runner) {
	const gameStatus = await runner.exercise('game_data_status', {}, {
		memoryAfterFirst: 'after-catalogue-readiness',
	});
	const gameAvailable = gameStatus?.available === true;
	runner.corpus.gameData = gameStatus ? {
		catalogueRevision: gameStatus.catalogueRevision,
		scopeRevision: gameStatus.scopeRevision,
		scopeAuthority: gameStatus.scopeAuthority,
		counts: gameStatus.counts,
		coverage: gameStatus.coverage,
		addons: Array.isArray(gameStatus.addons) ? gameStatus.addons.map(addon => ({
			addonGuid: addon.addonGuid,
			available: addon.available,
			scriptCount: addon.scriptCount,
		})) : [],
	} : undefined;
	let gameSearch;
	let gameResearch;
	if (gameAvailable) {
		gameSearch = await runner.exercise('search_game_data_symbols', {
			query: runner.configuration.gameSymbolQuery,
			limit: 20,
		}, { memoryAfterFirst: 'after-ordinary-semantic-search' });
		const gameText = await runner.exercise('search_game_data_text', {
			query: runner.configuration.textQuery,
			limit: 20,
		});
		await runTextVariants(runner, 'search_game_data_text', gameText);
		gameResearch = await runner.exercise('research_game_data', { query: runner.configuration.gameSymbolQuery }, {
			unavailableCodes: ['source_evidence_unavailable'],
		});
	} else {
		runner.skipMany([
			'search_game_data_symbols',
			'search_game_data_text',
			'research_game_data',
		], 'Game Data status reports that the catalogue is unavailable.');
	}
	await runner.exercise('search_game_data_resources', { query: runner.configuration.gameSymbolQuery, limit: 20 }, {
		unavailableCodes: ['resource_catalogue_unavailable', 'game_data_unavailable'],
	});
	await runner.exercise('search_reforger', { query: runner.configuration.gameSymbolQuery });
	const gameResult = firstResult(gameSearch);
	if (gameResult?.symbolRef) {
		await runGenericSymbolScenarios(runner, 'gameData', gameResult.symbolRef);
		await runner.exercise('inspect_game_data_symbol', { symbolRef: gameResult.symbolRef });
		await runner.exercise('list_game_data_symbol_members', { symbolRef: gameResult.symbolRef, limit: 20 });
		await runner.exercise('query_game_data_symbol_relationships', {
			symbolRef: gameResult.symbolRef,
			relationshipKinds: ['directBase', 'derivedType'],
			limit: 20,
		}, { unavailableCodes: ['source_evidence_unavailable'] });
	} else {
		runner.skipMany([
			'inspect_game_data_symbol',
			'list_game_data_symbol_members',
			'query_game_data_symbol_relationships',
		], gameAvailable ? 'The configured Game Data symbol query returned no symbol handoff.' : 'Game Data is unavailable.');
	}
	if (gameResult?.readSourceInput) {
		await runner.exercise('read_game_data_source', gameResult.readSourceInput);
		await runner.variant('read_game_data_source', { ...gameResult.readSourceInput, includePreview: true }, 'lexical-preview');
		const researchReadInput = gameResearch?.primary?.readSourceInput;
		if (researchReadInput) await runner.variant('read_game_data_source', researchReadInput, 'research-handoff');
	} else if (gameResearch?.primary?.readSourceInput) {
		await runner.exercise('read_game_data_source', gameResearch.primary.readSourceInput);
	} else {
		runner.skip('read_game_data_source', gameAvailable ? 'The configured Game Data symbol query returned no source-read handoff.' : 'Game Data is unavailable.');
	}

	const workspaceSearch = await runner.exercise(
		'search_workspace_symbols',
		{ query: runner.configuration.workspaceSymbolQuery, limit: 20 },
		{ unavailableCodes: ['workspace_unavailable', 'workspace_index_unavailable'] },
	);
	const workspaceAvailable = workspaceSearch !== undefined;
	const workspaceText = await runner.exercise(
		'search_workspace_text',
		{ query: runner.configuration.textQuery, limit: 20 },
		{ unavailableCodes: ['workspace_unavailable', 'workspace_index_unavailable'] },
	);
	if (workspaceText !== undefined) await runTextVariants(runner, 'search_workspace_text', workspaceText);
	const workspaceResult = firstResult(workspaceSearch);
	runner.corpus.workspace = workspaceSearch ? {
		catalogueRevision: workspaceSearch.catalogueRevision,
		rootCount: runner.configuration.workspaceScripts.length,
		indexedSymbols: runner.configuration.workspaceIndexInventory?.indexedSymbols,
	} : undefined;
	if (gameAvailable && workspaceAvailable) {
		await runner.mixedSemanticSearch(
			{ query: runner.configuration.gameSymbolQuery, limit: 20 },
			{ query: runner.configuration.workspaceSymbolQuery, limit: 20 },
		);
	}
	if (workspaceResult?.symbolRef) {
		await runGenericSymbolScenarios(runner, 'workspace', workspaceResult.symbolRef);
		await runner.exercise('inspect_workspace_symbol', { symbolRef: workspaceResult.symbolRef });
		await runner.exercise('list_workspace_symbol_members', { symbolRef: workspaceResult.symbolRef, limit: 20 });
		await runner.exercise('query_workspace_symbol_relationships', {
			symbolRef: workspaceResult.symbolRef,
			relationshipKinds: ['reference'],
			limit: 20,
		});
	} else {
		runner.skipMany([
			'inspect_workspace_symbol',
			'list_workspace_symbol_members',
			'query_workspace_symbol_relationships',
		], workspaceAvailable ? 'The configured workspace symbol query returned no symbol handoff.' : 'Workspace source is unavailable.');
	}
	if (workspaceResult?.readSourceInput) {
		await runner.exercise('read_workspace_source', workspaceResult.readSourceInput);
		await runner.variant('read_workspace_source', { ...workspaceResult.readSourceInput, includePreview: true }, 'lexical-preview');
	} else {
		runner.skip('read_workspace_source', workspaceAvailable ? 'The configured workspace symbol query returned no source-read handoff.' : 'Workspace source is unavailable.');
	}
	if (!gameResult?.symbolRef && !workspaceResult?.symbolRef) {
		runner.skipMany(['inspect_symbol', 'list_symbol_members', 'query_symbol_relationships'], 'No exact symbol handoff is available.');
	}

	const relationshipAnchor = gameResult?.symbolRef
		? { anchorSource: 'gameData', symbolRef: gameResult.symbolRef }
		: workspaceResult?.symbolRef
			? { anchorSource: 'workspace', symbolRef: workspaceResult.symbolRef }
			: undefined;
	if (relationshipAnchor) {
		const addonGuids = Array.isArray(gameStatus?.addons)
			? gameStatus.addons
				.filter(addon => addon?.available !== false && typeof addon?.addonGuid === 'string')
				.map(addon => addon.addonGuid)
				.sort()
			: [];
		const relationshipBase = {
			...relationshipAnchor,
			includeWorkspace: workspaceAvailable,
			addonGuids,
			depth: 'one',
			limit: 20,
		};
		await runner.exercise('query_source_symbol_relationships', {
			...relationshipBase,
			relationshipKinds: ['directBase', 'derivedType'],
		}, {
			scenario: 'one-level-hierarchy',
			memoryAfterFirst: 'after-first-relationship-projection',
			memoryAfterWarm: 'after-repeated-relationship-query',
		});
		await runner.variant('query_source_symbol_relationships', {
			...relationshipBase,
			relationshipKinds: ['direct'],
		}, 'direct');
		await runner.variant('query_source_symbol_relationships', {
			...relationshipBase,
			depth: 'all',
			relationshipKinds: ['directBase', 'derivedType'],
		}, 'all-level-hierarchy');
		const fanout = await runner.variant('query_source_symbol_relationships', {
			...relationshipBase,
			depth: 'all',
			limit: 1,
			relationshipKinds: ['derivedType'],
		}, 'broad-descendant-fanout');
		if (fanout?.nextCursor) {
			await runner.variant('query_source_symbol_relationships', {
				...relationshipBase,
				depth: 'all',
				limit: 1,
				relationshipKinds: ['derivedType'],
				cursor: fanout.nextCursor,
			}, 'later-page-retrieval');
		}
		await runner.variant('query_source_symbol_relationships', {
			...relationshipBase,
			relationshipKinds: ['moddedExtension'],
		}, 'modded-extensions');
		await runner.variant('query_source_symbol_relationships', {
			...relationshipBase,
			includeWorkspace: false,
			relationshipKinds: ['direct'],
		}, 'scope-change');
		await runner.cancellation('query_source_symbol_relationships', {
			...relationshipBase,
			depth: 'all',
			limit: 100,
			relationshipKinds: ['derivedType', 'moddedExtension'],
		});
		if (gameAvailable) {
			const methodSearch = await runner.variant('search_game_data_symbols', {
				query: runner.configuration.relationshipMethodQuery,
				kinds: ['method'],
				limit: 20,
			}, 'relationship-method-discovery');
			const method = firstResult(methodSearch);
			if (method?.symbolRef) {
				const methodBase = {
					...relationshipBase,
					anchorSource: 'gameData',
					symbolRef: method.symbolRef,
					depth: 'all',
				};
				const overrides = await runner.variant('query_source_symbol_relationships', {
					...methodBase,
					relationshipKinds: ['override'],
				}, 'overrides');
				const downstream = firstResult(overrides);
				await runner.variant('query_source_symbol_relationships', {
					...methodBase,
					anchorSource: downstream?.source ?? methodBase.anchorSource,
					symbolRef: downstream?.symbolRef ?? methodBase.symbolRef,
					relationshipKinds: ['overriddenDeclaration'],
				}, 'base-implementations');
			}
		}
	} else {
		runner.skip('query_source_symbol_relationships', 'No exact workspace or Game Data anchor was returned by discovery.');
	}

	const wikiStatus = await runner.exercise('official_wiki_status', {});
	let wikiSearch;
	if (wikiStatus?.available === true) {
		wikiSearch = await runner.exercise('search_official_wiki', {
			query: runner.configuration.wikiQuery,
			limit: 20,
		});
	} else {
		runner.skip('search_official_wiki', 'Official Wiki status reports that the corpus is unavailable.');
	}
	const wikiResult = firstResult(wikiSearch);
	if (wikiResult?.readInput) {
		await runner.exercise('read_official_wiki', wikiResult.readInput);
	} else {
		runner.skip('read_official_wiki', wikiStatus?.available === true ? 'The configured Wiki query returned no read handoff.' : 'Official Wiki is unavailable.');
	}
}

async function runGenericSymbolScenarios(runner, source, symbolRef) {
	const scenarios = [
		['inspect_symbol', { source, symbolRef }],
		['list_symbol_members', { source, symbolRef, limit: 20 }],
		['query_symbol_relationships', { source, symbolRef, relationshipKinds: ['reference'], limit: 20 }],
	];
	for (const [name, argumentsValue] of scenarios) {
		const operation = runner.operations.find(candidate => candidate.name === name);
		if (operation) {
			await runner.variant(name, argumentsValue, source);
		} else {
			await runner.exercise(name, argumentsValue, { scenario: source });
		}
	}
}

async function runTextVariants(runner, name, firstPage) {
	await runner.variant(name, {
		query: runner.configuration.broadTextQuery,
		limit: 20,
	}, 'broad-literal');
	await runner.variant(name, {
		query: runner.configuration.regexQuery,
		useRegex: true,
		limit: 20,
	}, 'regular-expression');
	if (firstPage?.nextCursor) {
		await runner.variant(name, {
			query: runner.configuration.textQuery,
			limit: 20,
			cursor: firstPage.nextCursor,
		}, 'pagination');
	}
}

async function measureColdProcesses(configuration) {
	const initializeSamples = [];
	const statusSamples = [];
	const navigationSamples = [];
	const sourceReadSamples = [];
	let missingNavigationHandoffs = 0;
	let availableCount = 0;
	let failed = 0;
	for (let iteration = 0; iteration < configuration.coldSamples; iteration += 1) {
		const client = new McpClient(resolve(configuration.server), serverArguments(configuration), configuration.timeoutMs);
		try {
			await client.initialize();
			initializeSamples.push(client.processElapsedMs());
			const listed = await client.listTools();
			if ((listed.tools ?? []).some(tool => tool.name === 'game_data_status')) {
				const status = await measure(() => client.callTool('game_data_status', {}));
				if (status.value?.isError === true) {
					failed += 1;
				} else {
					statusSamples.push(status.elapsedMs);
					if (status.value?.structuredContent?.available === true) {
						availableCount += 1;
						const started = performance.now();
						const search = await client.callTool('search_game_data_symbols', { query: configuration.gameSymbolQuery, limit: 20 });
						const hit = firstResult(search?.structuredContent);
						if (search?.isError === true) {
							failed += 1;
						} else if (hit?.symbolRef && hit?.readSourceInput) {
							const inspection = await client.callTool('inspect_symbol', { source: 'gameData', symbolRef: hit.symbolRef });
							const read = await measure(() => client.callTool('read_game_data_source', hit.readSourceInput));
							if (inspection?.isError === true || read.value?.isError === true) {
								failed += 1;
							} else {
								navigationSamples.push(performance.now() - started);
								sourceReadSamples.push(read.elapsedMs);
							}
						} else {
							missingNavigationHandoffs += 1;
						}
					}
				}
			}
		} catch {
			failed += 1;
		} finally {
			await client.close();
		}
	}
	return {
		count: configuration.coldSamples,
		processToInitialize: distribution(initializeSamples),
		gameDataStatus: distribution(statusSamples),
		gameDataAvailable: availableCount,
		firstNavigation: distribution(navigationSamples),
		firstSourceRead: distribution(sourceReadSamples),
		missingNavigationHandoffs,
		failed,
	};
}

class McpClient {
	constructor(command, argumentsValue, timeoutMs) {
		this.processStartedAt = performance.now();
		this.timeoutMs = timeoutMs;
		this.nextId = 1;
		this.pending = new Map();
		this.stderrTail = '';
		this.child = spawn(command, argumentsValue, {
			stdio: ['pipe', 'pipe', 'pipe'],
			windowsHide: true,
		});
		this.child.stderr.setEncoding('utf8');
		this.child.stderr.on('data', chunk => {
			this.stderrTail = (this.stderrTail + chunk).slice(-2048);
		});
		this.child.on('error', error => this.rejectPending(error));
		this.child.on('exit', code => {
			if (this.pending.size > 0) this.rejectPending(new Error(`MCP server exited with code ${code}.`));
		});
		this.lines = createInterface({ input: this.child.stdout });
		this.lines.on('line', line => this.acceptLine(line));
	}

	async initialize() {
		const value = await this.request('initialize', {
			protocolVersion: '2025-11-25',
			capabilities: {},
			clientInfo: { name: 'reforger-mcp-runtime-report', version: '1.0.0' },
		}, 'initialize');
		this.send({ jsonrpc: '2.0', method: 'notifications/initialized' });
		return value;
	}

	listTools() {
		return this.request('tools/list', {}, 'tools/list');
	}

	callTool(name, argumentsValue) {
		return this.request('tools/call', { name, arguments: argumentsValue }, name);
	}

	async cancelTool(name, argumentsValue) {
		const pending = this.beginRequest('tools/call', { name, arguments: argumentsValue }, name);
		this.send({
			jsonrpc: '2.0',
			method: 'notifications/cancelled',
			params: { requestId: pending.id, reason: 'performance cancellation probe' },
		});
		const outcome = await Promise.race([
			pending.promise.then(value => value?.isError === true ? 'cancelled-response' : 'completed-before-cancel'),
			new Promise(resolve => setTimeout(() => resolve('no-stale-response'), 250)),
		]);
		if (outcome === 'no-stale-response') this.abandon(pending.id);
		return outcome;
	}

	processElapsedMs() {
		return performance.now() - this.processStartedAt;
	}

	processId() {
		return this.child.pid;
	}

	request(method, params, phase) {
		return this.beginRequest(method, params, phase).promise;
	}

	beginRequest(method, params, phase) {
		const id = this.nextId++;
		const promise = new Promise((resolveRequest, reject) => {
			const timer = setTimeout(() => {
				this.pending.delete(id);
				reject(Object.assign(new Error(`MCP request exceeded ${this.timeoutMs} ms.`), { phase }));
			}, this.timeoutMs);
			this.pending.set(id, { resolveRequest, reject, timer, phase });
			this.send({ jsonrpc: '2.0', id, method, params });
		});
		return { id, promise };
	}

	abandon(id) {
		const pending = this.pending.get(id);
		if (!pending) return;
		clearTimeout(pending.timer);
		this.pending.delete(id);
	}

	send(message) {
		this.child.stdin.write(`${JSON.stringify(message)}\n`);
	}

	acceptLine(line) {
		let message;
		try {
			message = JSON.parse(line);
		} catch {
			return;
		}
		const pending = this.pending.get(message.id);
		if (!pending) return;
		this.pending.delete(message.id);
		clearTimeout(pending.timer);
		if (message.error) {
			pending.reject(Object.assign(new Error(message.error.message ?? 'MCP protocol error.'), {
				phase: pending.phase,
				code: message.error.code,
			}));
		} else {
			pending.resolveRequest(message.result);
		}
	}

	rejectPending(error) {
		for (const pending of this.pending.values()) {
			clearTimeout(pending.timer);
			pending.reject(error);
		}
		this.pending.clear();
	}

	async close() {
		this.lines?.close();
		this.child.stdin.end();
		if (this.child.exitCode !== null) return;
		const exited = new Promise(resolveExit => this.child.once('exit', resolveExit));
		this.child.kill();
		await Promise.race([
			exited,
			new Promise(resolveTimeout => setTimeout(resolveTimeout, 250)),
		]);
	}
}

function parseArguments(args) {
	const parsed = {
		serverPrefixArgs: [],
		workspaceScripts: [],
		dependencyProjects: [],
		externalIndexMode: 'loaded',
		buildProfile: 'release',
		samples: 7,
		coldSamples: 3,
		concurrencyLevels: [1, 4, 8],
		timeoutMs: 120_000,
		jsonOut: DEFAULT_JSON_OUT,
		markdownOut: DEFAULT_MARKDOWN_OUT,
		gameSymbolQuery: 'SCR_BaseGameMode',
		workspaceSymbolQuery: 'SCR_',
		textQuery: 'SCR_BaseGameMode',
		broadTextQuery: 'class',
		regexQuery: '\\bclass\\s+[A-Za-z_][A-Za-z0-9_]*',
		wikiQuery: 'replication',
		relationshipMethodQuery: 'OnActivate',
		commit: currentCommit(),
		requireAll: false,
		enforceBudgets: false,
		acceptProjectionMemory: false,
	};
	for (let index = 0; index < args.length; index += 1) {
		const argument = args[index];
		const value = () => {
			const next = args[++index];
			if (!next) usage(`Missing value for ${argument}.`);
			return next;
		};
		switch (argument) {
			case '--server': parsed.server = value(); break;
			case '--server-prefix-arg': parsed.serverPrefixArgs.push(value()); break;
			case '--addon-index-storage': parsed.addonIndexStorage = value(); break;
			case '--addon-source-inventory': parsed.addonSourceInventory = value(); break;
			case '--external-index-mode': parsed.externalIndexMode = value(); break;
			case '--build-profile': parsed.buildProfile = value(); break;
			case '--workspace-scripts': parsed.workspaceScripts.push(value()); break;
			case '--workspace-index-inventory': parsed.workspaceIndexInventoryPath = value(); break;
			case '--dependency-project': parsed.dependencyProjects.push(value()); break;
			case '--official-wiki-root': parsed.officialWikiRoot = value(); break;
			case '--samples': parsed.samples = nonNegativeInteger(value(), argument); break;
			case '--cold-samples': parsed.coldSamples = nonNegativeInteger(value(), argument); break;
			case '--concurrency': parsed.concurrencyLevels = boundedConcurrencyLevels(value(), argument); break;
			case '--concurrency-levels': parsed.concurrencyLevels = boundedConcurrencyLevels(value(), argument); break;
			case '--timeout-ms': parsed.timeoutMs = positiveInteger(value(), argument); break;
			case '--json-out': parsed.jsonOut = value(); break;
			case '--markdown-out': parsed.markdownOut = value(); break;
			case '--game-symbol-query': parsed.gameSymbolQuery = value(); break;
			case '--workspace-symbol-query': parsed.workspaceSymbolQuery = value(); break;
			case '--text-query': parsed.textQuery = value(); break;
			case '--broad-text-query': parsed.broadTextQuery = value(); break;
			case '--regex-query': parsed.regexQuery = value(); break;
			case '--wiki-query': parsed.wikiQuery = value(); break;
			case '--relationship-method-query': parsed.relationshipMethodQuery = value(); break;
			case '--commit': parsed.commit = value(); break;
			case '--baseline-report': parsed.baselineReport = value(); break;
			case '--paired-baseline-server': parsed.pairedBaselineServer = value(); break;
			case '--comparison-json-out': parsed.comparisonJsonOut = value(); break;
			case '--comparison-markdown-out': parsed.comparisonMarkdownOut = value(); break;
			case '--require-all': parsed.requireAll = true; break;
			case '--enforce-budgets': parsed.enforceBudgets = true; break;
			case '--accept-projection-memory': parsed.acceptProjectionMemory = true; break;
			case '--help': usage(); break;
			default: usage(`Unknown argument: ${argument}`);
		}
	}
	if (!parsed.server) usage('--server is required.');
	if (!['all', 'loaded', 'none'].includes(parsed.externalIndexMode)) {
		usage('--external-index-mode must be all, loaded, or none.');
	}
	if (!['release', 'debug'].includes(parsed.buildProfile)) {
		usage('--build-profile must be release or debug.');
	}
	if (parsed.workspaceIndexInventoryPath) {
		parsed.workspaceIndexInventory = parseWorkspaceIndexInventory(parsed.workspaceIndexInventoryPath);
	}
	return parsed;
}

function parseWorkspaceIndexInventory(path) {
	const absolutePath = resolve(path);
	let markdown;
	try {
		markdown = readFileSync(absolutePath, 'utf8');
	} catch (error) {
		usage(`Unable to read --workspace-index-inventory ${absolutePath}: ${error.message}`);
	}
	const match = markdown.match(/^\| `workspace` \| \d+ \| \d+ \| (\d+) \| \d+ \|\s*$/im);
	if (!match) {
		usage('--workspace-index-inventory must be an index_build_baseline Markdown report containing a workspace Source Roots row.');
	}
	return {
		path: absolutePath,
		sha256: createHash('sha256').update(markdown).digest('hex'),
		indexedSymbols: Number.parseInt(match[1], 10),
	};
}

function serverArguments(configuration) {
	const args = [...configuration.serverPrefixArgs, 'mcp', '--tool-profile', 'all'];
	if (configuration.addonSourceInventory) args.push('--addon-source-inventory', resolve(configuration.addonSourceInventory));
	if (configuration.addonIndexStorage) args.push('--addon-index-storage', resolve(configuration.addonIndexStorage));
	args.push('--external-index-mode', configuration.externalIndexMode);
	for (const root of configuration.workspaceScripts) args.push('--workspace-scripts', resolve(root));
	for (const project of configuration.dependencyProjects) args.push('--dependency-project', resolve(project));
	if (configuration.officialWikiRoot) args.push('--official-wiki-root', resolve(configuration.officialWikiRoot));
	return args;
}

function summarizeResult(result) {
	const structured = result?.structuredContent ?? {};
	return {
		isError: result?.isError === true,
		code: typeof structured.code === 'string' ? structured.code : undefined,
		message: typeof structured.message === 'string' ? structured.message : undefined,
		structured,
		public: {
			code: typeof structured.code === 'string' ? structured.code : undefined,
			returned: finiteNumber(structured.returned),
			total: finiteNumber(structured.total),
			truncated: typeof structured.truncated === 'boolean' ? structured.truncated : undefined,
			stats: numericObject(structured.stats),
		},
	};
}

function resultFingerprint(result) {
	const structured = redactContent(result?.structuredContent ?? {});
	return createHash('sha256').update(JSON.stringify(structured)).digest('hex').slice(0, 16);
}

function redactContent(value, key = '') {
	if (['content', 'excerpt', 'lineText', 'text'].includes(key)) return '<redacted>';
	if (key === 'stats') return '<volatile-diagnostics>';
	if (/(?:ms|timings)$/i.test(key)) return '<volatile>';
	if (Array.isArray(value)) return value.map(item => redactContent(item));
	if (value && typeof value === 'object') {
		return Object.fromEntries(Object.entries(value).map(([childKey, child]) => [childKey, redactContent(child, childKey)]));
	}
	return value;
}

function responseBytes(result) {
	return result === undefined ? 0 : Buffer.byteLength(JSON.stringify(result));
}

function firstResult(page) {
	return Array.isArray(page?.results) ? page.results[0] : undefined;
}

function operationBudget(name) {
	if (name === 'game_data_status') return GAME_DATA_INITIALIZATION_BUDGET_MS;
	if (name.includes('_text')) return TEXT_SEARCH_BUDGET_MS;
	if (name.includes('official_wiki')) return WIKI_BUDGET_MS;
	return SEMANTIC_BUDGET_MS;
}

function evaluateRelationshipGates(operations) {
	const operation = operations.find(candidate => candidate.name === 'query_source_symbol_relationships');
	if (!operation || ['missing', 'skipped'].includes(operation.status)) return [];
	const gates = [
		thresholdGate('first-lazy', operation.firstMs, RELATIONSHIP_FIRST_BUDGET_MS, 'max'),
		thresholdGate('cached-one-level', operation.warm.medianMs, RELATIONSHIP_ONE_LEVEL_MEDIAN_BUDGET_MS, 'median'),
	];
	for (const scenario of ['all-level-hierarchy', 'broad-descendant-fanout']) {
		const variant = operation.variants?.find(candidate => candidate.scenario === scenario);
		if (variant) gates.push(thresholdGate(scenario, variant.warm.p95Ms, RELATIONSHIP_BROAD_P95_BUDGET_MS, 'p95'));
	}
	return gates;
}

function thresholdGate(name, actualMs, budgetMs, statistic) {
	return { name, statistic, actualMs: round(actualMs), budgetMs, status: actualMs <= budgetMs ? 'pass' : 'fail' };
}

function captureWorkingSet(client, destination, stage) {
	const pid = client.processId();
	if (!Number.isInteger(pid)) return;
	try {
		let workingSetBytes;
		if (process.platform === 'win32') {
			const output = execFileSync('powershell.exe', [
				'-NoProfile',
				'-NonInteractive',
				'-Command',
				`(Get-Process -Id ${pid} -ErrorAction Stop).WorkingSet64`,
			], { encoding: 'utf8', windowsHide: true, timeout: 5_000 });
			workingSetBytes = Number.parseInt(output.trim(), 10);
		} else {
			const status = readFileSync(`/proc/${pid}/status`, 'utf8');
			const match = /^VmRSS:\s+(\d+)\s+kB$/m.exec(status);
			workingSetBytes = match ? Number.parseInt(match[1], 10) * 1024 : undefined;
		}
		if (Number.isFinite(workingSetBytes)) destination.push({ stage, workingSetBytes });
	} catch (error) {
		destination.push({ stage, unavailable: sanitizeMessage(error.message) });
	}
}

function currentCommit() {
	try {
		return execFileSync('git', ['rev-parse', 'HEAD'], { encoding: 'utf8', windowsHide: true }).trim();
	} catch {
		return 'unknown';
	}
}

function toolFamily(name) {
	if (name.includes('official_wiki')) return 'Official Wiki';
	if (name.includes('workspace')) return name.includes('_text') ? 'Workspace text' : 'Workspace semantic/source';
	if (name.includes('_text')) return 'Game Data text';
	return 'Game Data semantic/source';
}

function isConcurrencyCandidate(name) {
	return ['search_game_data_symbols', 'search_workspace_symbols', 'search_official_wiki'].includes(name);
}

function skippedOperation(name, reason, status = 'skipped') {
	return {
		name,
		family: toolFamily(name),
		status,
		reason: sanitizeMessage(reason),
		firstMs: 0,
		warm: emptyDistribution(),
		responseBytes: 0,
	};
}

function distribution(values) {
	if (values.length === 0) return emptyDistribution();
	const sorted = [...values].sort((left, right) => left - right);
	return {
		count: sorted.length,
		minMs: round(sorted[0]),
		medianMs: round(percentile(sorted, 50)),
		p95Ms: round(percentile(sorted, 95)),
		maxMs: round(sorted.at(-1)),
	};
}

function emptyDistribution() {
	return { count: 0, minMs: 0, medianMs: 0, p95Ms: 0, maxMs: 0 };
}

function percentile(sorted, percentileValue) {
	return sorted[Math.min(sorted.length - 1, Math.ceil(sorted.length * percentileValue / 100) - 1)];
}

async function measure(operation) {
	const started = performance.now();
	const value = await operation();
	return { elapsedMs: performance.now() - started, value };
}

function renderMarkdown(report) {
	const lines = [
		'# MCP Runtime Performance Report',
		'',
		'This source-free report exercises the live non-Workbench MCP catalogue through stdio. Timings are local wall-clock diagnostics, not portable benchmark gates.',
		'',
		'## Summary',
		'',
		`- Generated: ${report.generatedAt}`,
		`- Verdict: **${report.verdict}**`,
		`- API coverage: **${report.coverage.exercised} / ${report.coverage.listed}**`,
		`- Skipped or unavailable: ${report.coverage.skipped}`,
		`- Correctness/gated failures: ${report.coverage.failed}`,
		`- Over-budget operations: ${report.coverage.overBudget}${report.configuration.enforceBudgets ? ' (enforced)' : ' (trend only)'}`,
		`- Initialize: ${formatMs(report.protocol.initializeMs)}`,
		`- Process start to initialize: ${formatMs(report.protocol.processToInitializeMs)}`,
		`- tools/list: ${formatMs(report.protocol.toolsListMs)}`,
		`- Commit: ${report.configuration.commit}`,
		'',
		'## Controlled Inputs',
		'',
		`- Host: ${report.host.hostname}; ${report.host.platform} ${report.host.release}; ${report.host.architecture}; ${report.host.logicalCpuCount} logical CPUs; ${report.host.totalMemoryBytes} bytes RAM`,
		`- Build profile: ${report.configuration.buildProfile}`,
		`- Tool profile: ${report.configuration.toolProfile}`,
		`- Samples: ${report.configuration.samples} warm; ${report.configuration.coldSamples} cold; concurrency ${report.configuration.concurrencyLevels.join('/')}; timeout ${report.configuration.timeoutMs} ms`,
		`- Workspace roots: ${report.configuration.workspaceRoots.join(', ') || 'None'}`,
		`- Game Data revision: ${report.corpus.gameData?.catalogueRevision ?? 'Unavailable'}`,
		`- Game Data scope revision / authority: ${report.corpus.gameData?.scopeRevision ?? 'Unavailable'} / ${report.corpus.gameData?.scopeAuthority ?? 'Unavailable'}`,
		`- Game Data indexed symbols: ${report.corpus.gameData?.coverage?.indexedSymbols ?? 'Unavailable'}`,
		`- Workspace revision: ${report.corpus.workspace?.catalogueRevision ?? 'Unavailable'}`,
		`- Workspace indexed symbols: ${report.corpus.workspace?.indexedSymbols ?? 'Unavailable in this binary'}`,
		`- Selected add-ons: ${(report.corpus.gameData?.addons ?? []).map(addon => addon.addonGuid).join(', ') || 'None'}`,
		`- Queries: ${Object.entries(report.configuration.queries ?? {}).map(([name, value]) => `${name}=${JSON.stringify(value)}`).join('; ')}`,
		'',
		'## API Scorecard',
		'',
		'| Tool | Family | Status | First | Warm median | Warm p95 | Max | Bytes | Budget |',
		'| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |',
	];
	for (const operation of report.operations) {
		lines.push(`| ${escapeMarkdown(operation.name)} | ${escapeMarkdown(operation.family)} | ${escapeMarkdown(operation.status)} | ${formatMs(operation.firstMs)} | ${formatMs(operation.warm.medianMs)} | ${formatMs(operation.warm.p95Ms)} | ${formatMs(operation.warm.maxMs)} | ${operation.responseBytes} | ${operation.budgetMs ? formatMs(operation.budgetMs) : ''} |`);
	}
	const variants = report.operations.flatMap(operation => (operation.variants ?? []).map(variant => ({ tool: operation.name, ...variant })));
	lines.push('', '## Scenario Variants', '');
	if (variants.length === 0) {
		lines.push('No additional handoff or search variants were available.');
	} else {
		lines.push('| Tool | Scenario | Status | Last | Median | P95 | Bytes | Returned | Total |', '| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |');
		for (const variant of variants) {
			lines.push(`| ${escapeMarkdown(variant.tool)} | ${escapeMarkdown(variant.scenario)} | ${escapeMarkdown(variant.status)} | ${formatMs(variant.elapsedMs)} | ${formatMs(variant.warm?.medianMs)} | ${formatMs(variant.warm?.p95Ms)} | ${variant.responseBytes} | ${variant.returned ?? ''} | ${variant.total ?? ''} |`);
		}
	}
	lines.push('', '## Relationship Gates', '');
	if (report.relationshipGates.length === 0) {
		lines.push('The composed relationship tool was unavailable in this binary.');
	} else {
		lines.push('| Scenario | Statistic | Actual | Budget | Status |', '| --- | --- | ---: | ---: | --- |');
		for (const gate of report.relationshipGates) {
			lines.push(`| ${gate.name} | ${gate.statistic} | ${formatMs(gate.actualMs)} | ${formatMs(gate.budgetMs)} | ${gate.status} |`);
		}
	}
	lines.push('', '## Process Working Set', '', '| Stage | Bytes | MiB |', '| --- | ---: | ---: |');
	for (const sample of report.memory) {
		lines.push(`| ${sample.stage} | ${sample.workingSetBytes ?? 'Unavailable'} | ${sample.workingSetBytes ? round(sample.workingSetBytes / 1024 / 1024) : ''} |`);
	}
	lines.push('', '## Coverage Notes', '');
	const incomplete = report.operations.filter(operation => operation.reason);
	if (incomplete.length === 0) {
		lines.push('Every listed non-Workbench tool completed its configured scenario.');
	} else {
		for (const operation of incomplete) lines.push(`- \`${operation.name}\`: ${operation.reason}`);
	}
	lines.push(
		'',
		'## Cold Process Samples',
		'',
		`- Processes: ${report.coldProcess.count}`,
		`- Start-to-initialize median / p95: ${formatMs(report.coldProcess.processToInitialize.medianMs)} / ${formatMs(report.coldProcess.processToInitialize.p95Ms)}`,
		`- First Game Data status median / p95: ${formatMs(report.coldProcess.gameDataStatus.medianMs)} / ${formatMs(report.coldProcess.gameDataStatus.p95Ms)}`,
		`- First navigation (search, inspect, source read) median / p95: ${formatMs(report.coldProcess.firstNavigation.medianMs)} / ${formatMs(report.coldProcess.firstNavigation.p95Ms)}`,
		`- First source read median / p95: ${formatMs(report.coldProcess.firstSourceRead.medianMs)} / ${formatMs(report.coldProcess.firstSourceRead.p95Ms)}`,
		`- Missing navigation handoffs: ${report.coldProcess.missingNavigationHandoffs}`,
		`- Game Data available: ${report.coldProcess.gameDataAvailable} / ${report.coldProcess.count}`,
		`- Failed samples: ${report.coldProcess.failed}`,
		'',
		'## Concurrency Probe',
		'',
		`- Operation: ${report.concurrency.operation ?? 'Unavailable'}`,
		'',
		'| Requested | Completed | Failed | Batch | Individual p95 |',
		'| ---: | ---: | ---: | ---: | ---: |',
	);
	for (const probe of report.concurrency.probes) {
		lines.push(`| ${probe.requested} | ${probe.completed} | ${probe.failed} | ${formatMs(probe.elapsedMs)} | ${formatMs(probe.individual.p95Ms)} |`);
	}
	lines.push(
		'',
		'## Interpretation',
		'',
		'- `first` is the first invocation in the primary MCP process after prerequisite status or search handoffs.',
		'- Cold samples use fresh processes but do not claim a cold operating-system filesystem cache.',
		'- `warm` summarizes repeated identical calls in the same process.',
		'- Response bytes include the complete MCP result, while this report stores no retrieved source or documentation text.',
		'- A skipped API means its required authority or handoff was unavailable; it is not counted as a fast success.',
		'',
	);
	return `${lines.join('\n')}\n`;
}

function writeReport(path, contents) {
	const absolute = resolve(path);
	mkdirSync(dirname(absolute), { recursive: true });
	writeFileSync(absolute, contents, 'utf8');
}

function numericObject(value) {
	if (!value || typeof value !== 'object' || Array.isArray(value)) return undefined;
	const entries = Object.entries(value).filter(([, item]) => typeof item === 'number' && Number.isFinite(item));
	return entries.length > 0 ? Object.fromEntries(entries) : undefined;
}

function finiteNumber(value) {
	return typeof value === 'number' && Number.isFinite(value) ? value : undefined;
}

function sanitizeMessage(value) {
	return String(value ?? '').replace(/[A-Za-z]:\\[^\s]+/g, '<path>').slice(0, 300);
}

function toolErrorReason(code) {
	return code ? `Tool reported ${code}.` : 'Tool returned a structured error without a stable code.';
}

function formatMs(value) {
	return `${round(value)} ms`;
}

function escapeMarkdown(value) {
	return String(value).replaceAll('|', '\\|').replaceAll('\n', ' ');
}

function round(value) {
	return Math.round((Number(value) || 0) * 100) / 100;
}

function positiveInteger(value, argument) {
	const parsed = Number.parseInt(value, 10);
	if (!Number.isInteger(parsed) || parsed < 1) usage(`${argument} must be a positive integer.`);
	return parsed;
}

function nonNegativeInteger(value, argument) {
	const parsed = Number.parseInt(value, 10);
	if (!Number.isInteger(parsed) || parsed < 0) usage(`${argument} must be a non-negative integer.`);
	return parsed;
}

function boundedConcurrencyLevels(value, argument) {
	const levels = value.split(',').map(item => positiveInteger(item.trim(), argument));
	if (levels.some(level => level > 8)) usage(`${argument} levels must be between 1 and 8.`);
	return [...new Set(levels)].sort((left, right) => left - right);
}

function controlledInputs(report) {
	const configuration = report.configuration ?? {};
	const gameData = report.corpus?.gameData ?? {};
	const workspace = report.corpus?.workspace ?? {};
	return {
		host: report.host,
		buildProfile: configuration.buildProfile,
		toolProfile: configuration.toolProfile,
		sampling: {
			samples: configuration.samples,
			coldSamples: configuration.coldSamples,
			concurrencyLevels: configuration.concurrencyLevels,
			timeoutMs: configuration.timeoutMs,
		},
		sources: {
			externalIndexMode: configuration.externalIndexMode,
			workspaceRoots: configuration.workspaceRoots,
			dependencyProjects: configuration.dependencyProjects,
			addonSourceInventory: configuration.addonSourceInventory,
			addonIndexStorage: configuration.addonIndexStorage,
			officialWikiRoot: configuration.officialWikiRoot,
		},
		queries: configuration.queries,
		corpus: {
			gameDataCatalogueRevision: gameData.catalogueRevision,
			gameDataScopeRevision: gameData.scopeRevision,
			gameDataScopeAuthority: gameData.scopeAuthority,
			gameDataIndexedSymbols: gameData.coverage?.indexedSymbols,
			addons: (gameData.addons ?? [])
				.map(addon => ({
					addonGuid: addon.addonGuid,
					available: addon.available,
					scriptCount: addon.scriptCount,
				}))
				.sort((left, right) => String(left.addonGuid).localeCompare(String(right.addonGuid))),
			workspaceCatalogueRevision: workspace.catalogueRevision,
			workspaceRootCount: workspace.rootCount,
			workspaceIndexedSymbols: workspace.indexedSymbols,
		},
	};
}

function compareReports(baseline, candidate) {
	const operations = [];
	const baselineByName = new Map((baseline.operations ?? []).map(operation => [operation.name, operation]));
	for (const current of candidate.operations ?? []) {
		if (current.name === 'query_source_symbol_relationships') continue;
		const before = baselineByName.get(current.name);
		if (!before || !['pass', 'over-budget'].includes(before.status) || !['pass', 'over-budget'].includes(current.status)) continue;
		const medianBudgetMs = Math.max(before.warm.medianMs + 5, before.warm.medianMs * 1.1);
		const p95BudgetMs = Math.max(before.warm.p95Ms + 10, before.warm.p95Ms * 1.2);
		const correctnessMatch = before.fingerprint === current.fingerprint
			&& before.returned === current.returned
			&& before.total === current.total;
		operations.push({
			name: current.name,
			baselineMedianMs: before.warm.medianMs,
			candidateMedianMs: current.warm.medianMs,
			medianBudgetMs: round(medianBudgetMs),
			baselineP95Ms: before.warm.p95Ms,
			candidateP95Ms: current.warm.p95Ms,
			p95BudgetMs: round(p95BudgetMs),
			baselineBytes: before.responseBytes,
			candidateBytes: current.responseBytes,
			correctnessMatch,
			status: correctnessMatch && current.warm.medianMs <= medianBudgetMs && current.warm.p95Ms <= p95BudgetMs
				? 'pass'
				: 'fail',
		});
	}
	const baselinePreUse = workingSetAt(baseline, 'after-ordinary-semantic-search');
	const candidatePreUse = workingSetAt(candidate, 'after-ordinary-semantic-search');
	const candidateAfterInitialize = workingSetAt(candidate, 'after-initialize');
	const preUseBudgetBytes = baselinePreUse === undefined
		? undefined
		: Math.max(baselinePreUse + 32 * 1024 * 1024, baselinePreUse * 1.1);
	const repeatedRelationshipBytes = workingSetAt(candidate, 'after-repeated-relationship-query');
	const loadedIndexFootprintBytes = candidatePreUse === undefined || candidateAfterInitialize === undefined
		? undefined
		: Math.max(0, candidatePreUse - candidateAfterInitialize);
	const retainedProjectionDeltaBytes = repeatedRelationshipBytes === undefined || candidatePreUse === undefined
		? undefined
		: Math.max(0, repeatedRelationshipBytes - candidatePreUse);
	const retainedProjectionRatio = loadedIndexFootprintBytes > 0 && retainedProjectionDeltaBytes !== undefined
		? retainedProjectionDeltaBytes / loadedIndexFootprintBytes
		: undefined;
	const projectionMemoryAccepted = candidate.configuration?.acceptProjectionMemory === true;
	const memory = {
		candidateAfterInitializeBytes: candidateAfterInitialize,
		baselinePreUseBytes: baselinePreUse,
		candidatePreUseBytes: candidatePreUse,
		preUseBudgetBytes: preUseBudgetBytes === undefined ? undefined : Math.round(preUseBudgetBytes),
		firstProjectionBytes: workingSetAt(candidate, 'after-first-relationship-projection'),
		repeatedRelationshipBytes,
		loadedIndexFootprintBytes,
		retainedProjectionDeltaBytes,
		retainedProjectionRatio: Number.isFinite(retainedProjectionRatio) ? round(retainedProjectionRatio) : undefined,
		compactnessStatus: !Number.isFinite(retainedProjectionRatio)
			? 'unavailable'
			: retainedProjectionRatio <= 0.25
				? 'pass'
				: projectionMemoryAccepted ? 'accepted' : 'human-acceptance-required',
		projectionMemoryAccepted,
		status: baselinePreUse === undefined || candidatePreUse === undefined
			? 'unavailable'
			: candidatePreUse <= preUseBudgetBytes ? 'pass' : 'fail',
	};
	const baselineControlledInputs = controlledInputs(baseline);
	const candidateControlledInputs = controlledInputs(candidate);
	const controlledInputMismatches = Object.keys(candidateControlledInputs)
		.filter(key => JSON.stringify(baselineControlledInputs[key]) !== JSON.stringify(candidateControlledInputs[key]));
	const workspaceCorpusExpected = (candidate.configuration?.workspaceRootCount ?? 0) > 0
		|| (baseline.configuration?.workspaceRootCount ?? 0) > 0;
	if (workspaceCorpusExpected
		&& (!Number.isFinite(baseline.corpus?.workspace?.indexedSymbols)
			|| !Number.isFinite(candidate.corpus?.workspace?.indexedSymbols))) {
		controlledInputMismatches.push('corpus.workspaceIndexedSymbols:missing');
	}
	const controlledInputsMatch = controlledInputMismatches.length === 0;
	const corpusMatch = !controlledInputMismatches.some(key => key === 'sources' || key.startsWith('corpus'));
	const failed = operations.filter(operation => operation.status === 'fail').length
		+ candidate.relationshipGates.filter(gate => gate.status === 'fail').length
		+ (candidate.paired?.operations ?? []).filter(operation => operation.status === 'fail').length
		+ (candidate.paired?.coldGate?.status === 'fail' ? 1 : 0)
		+ (memory.status === 'fail' ? 1 : 0)
		+ (memory.compactnessStatus === 'human-acceptance-required' ? 1 : 0)
		+ (controlledInputsMatch ? 0 : 1);
	return {
		schemaVersion: 2,
		generatedAt: new Date().toISOString(),
		baseline: { commit: baseline.configuration?.commit ?? 'unknown', reportGeneratedAt: baseline.generatedAt },
		candidate: { commit: candidate.configuration?.commit ?? 'unknown', reportGeneratedAt: candidate.generatedAt },
		method: {
			warmSamples: candidate.configuration.samples,
			coldSamples: candidate.configuration.coldSamples,
			medianGate: 'max(baseline + 5 ms, baseline * 1.10)',
			p95Gate: 'max(baseline + 10 ms, baseline * 1.20)',
		},
		corpusMatch,
		controlledInputsMatch,
		controlledInputMismatches,
		controlledInputs: candidateControlledInputs,
		symbolCounts: {
			gameData: candidate.corpus?.gameData?.coverage?.indexedSymbols,
			workspace: candidate.corpus?.workspace?.indexedSymbols,
		},
		operations,
		newRelationshipGates: candidate.relationshipGates,
		paired: candidate.paired,
		memory,
		verdict: failed === 0 ? 'pass' : 'fail',
	};
}

function workingSetAt(report, stage) {
	const sample = (report.memory ?? []).find(candidate => candidate.stage === stage);
	return Number.isFinite(sample?.workingSetBytes) ? sample.workingSetBytes : undefined;
}

function renderComparisonMarkdown(comparison) {
	const lines = [
		'# MCP Runtime Performance Comparison',
		'',
		`- Baseline commit: ${comparison.baseline.commit}`,
		`- Candidate commit: ${comparison.candidate.commit}`,
		`- Controlled corpus match: ${comparison.corpusMatch ? 'yes' : 'no'}`,
		`- All controlled inputs match: ${comparison.controlledInputsMatch ? 'yes' : `no (${comparison.controlledInputMismatches.join(', ')})`}`,
		`- Indexed symbols: Game Data ${comparison.symbolCounts.gameData ?? 'unavailable'}; Workspace ${comparison.symbolCounts.workspace ?? 'unavailable'}`,
		`- Verdict: **${comparison.verdict}**`,
		'',
		'## Existing Operations',
		'',
		'| Tool | Baseline median | Candidate median | Median gate | Baseline P95 | Candidate P95 | P95 gate | Fingerprint/counts | Status |',
		'| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- | --- |',
	];
	for (const operation of comparison.operations) {
		lines.push(`| ${operation.name} | ${formatMs(operation.baselineMedianMs)} | ${formatMs(operation.candidateMedianMs)} | ${formatMs(operation.medianBudgetMs)} | ${formatMs(operation.baselineP95Ms)} | ${formatMs(operation.candidateP95Ms)} | ${formatMs(operation.p95BudgetMs)} | ${operation.correctnessMatch ? 'match' : 'mismatch'} | ${operation.status} |`);
	}
	if (comparison.paired) {
		lines.push(
			'',
			'## Alternating Paired Samples',
			'',
			comparison.paired.method,
			'',
			'| Tool | Baseline median | Candidate median | Baseline P95 | Candidate P95 | Fingerprints | Status |',
			'| --- | ---: | ---: | ---: | ---: | --- | --- |',
		);
		for (const operation of comparison.paired.operations) {
			lines.push(`| ${operation.name} | ${formatMs(operation.baseline.medianMs)} | ${formatMs(operation.candidate.medianMs)} | ${formatMs(operation.baseline.p95Ms)} | ${formatMs(operation.candidate.p95Ms)} | ${operation.fingerprintsMatch ? 'match' : 'mismatch'} | ${operation.status} |`);
		}
		lines.push(
			'',
			`- Alternating cold pairs: ${comparison.paired.cold.baselineInitialize.count}`,
			`- Baseline process-to-initialize median / P95: ${formatMs(comparison.paired.cold.baselineInitialize.medianMs)} / ${formatMs(comparison.paired.cold.baselineInitialize.p95Ms)}`,
			`- Candidate process-to-initialize median / P95: ${formatMs(comparison.paired.cold.candidateInitialize.medianMs)} / ${formatMs(comparison.paired.cold.candidateInitialize.p95Ms)}`,
			`- Baseline tools/list median / P95: ${formatMs(comparison.paired.cold.baselineToolsList.medianMs)} / ${formatMs(comparison.paired.cold.baselineToolsList.p95Ms)}`,
			`- Candidate tools/list median / P95: ${formatMs(comparison.paired.cold.candidateToolsList.medianMs)} / ${formatMs(comparison.paired.cold.candidateToolsList.p95Ms)}`,
			`- Baseline Game Data readiness median / P95: ${formatMs(comparison.paired.cold.baselineStatus.medianMs)} / ${formatMs(comparison.paired.cold.baselineStatus.p95Ms)}`,
			`- Candidate Game Data readiness median / P95: ${formatMs(comparison.paired.cold.candidateStatus.medianMs)} / ${formatMs(comparison.paired.cold.candidateStatus.p95Ms)}`,
			`- Cold phase gates: process-to-initialize ${comparison.paired.coldGate.phases.processToInitialize.status}; tools/list ${comparison.paired.coldGate.phases.toolsList.status}; Game Data readiness ${comparison.paired.coldGate.phases.gameDataReadiness.status}; overall ${comparison.paired.coldGate.status}.`,
		);
	}
	lines.push(
		'',
		'## New Relationship Operations',
		'',
		'| Scenario | Statistic | Candidate | Gate | Status |',
		'| --- | --- | ---: | ---: | --- |',
	);
	for (const gate of comparison.newRelationshipGates) {
		lines.push(`| ${gate.name} | ${gate.statistic} | ${formatMs(gate.actualMs)} | ${formatMs(gate.budgetMs)} | ${gate.status} |`);
	}
	lines.push(
		'',
		'## Working Set',
		'',
		`- Baseline before Related Code: ${formatMiB(comparison.memory.baselinePreUseBytes)}`,
		`- Candidate before Related Code: ${formatMiB(comparison.memory.candidatePreUseBytes)}`,
		`- Pre-use gate: ${formatMiB(comparison.memory.preUseBudgetBytes)} (${comparison.memory.status})`,
		`- Candidate after first projection: ${formatMiB(comparison.memory.firstProjectionBytes)}`,
		`- Candidate after repeated query: ${formatMiB(comparison.memory.repeatedRelationshipBytes)}`,
		`- Loaded semantic-index footprint: ${formatMiB(comparison.memory.loadedIndexFootprintBytes)}`,
		`- Retained relationship delta: ${formatMiB(comparison.memory.retainedProjectionDeltaBytes)}`,
		`- Projection / loaded-index ratio: ${formatPercent(comparison.memory.retainedProjectionRatio)} (${comparison.memory.compactnessStatus})`,
		'',
		'Local wall-clock and working-set observations are comparative evidence for this host, not portable guarantees.',
		'',
	);
	return `${lines.join('\n')}\n`;
}

function formatMiB(value) {
	return Number.isFinite(value) ? `${round(value / 1024 / 1024)} MiB` : 'Unavailable';
}

function formatPercent(value) {
	return Number.isFinite(value) ? `${round(value * 100)}%` : 'Unavailable';
}

function usage(error) {
	if (error) process.stderr.write(`${error}\n\n`);
	process.stderr.write(
		'Usage: node tools/mcp-runtime-performance-report.mjs --server <executable> [options]\n' +
		'  Sources: --addon-index-storage <dir> --addon-source-inventory <json> --external-index-mode <all|loaded|none>\n' +
		'           --workspace-scripts <dir> (repeatable) --workspace-index-inventory <index-build-report.md>\n' +
		'           --dependency-project <gproj> (repeatable) --official-wiki-root <dir>\n' +
		'  Build: --build-profile <release|debug>\n' +
		'  Sampling: --samples <n> --cold-samples <n> --concurrency-levels <1,4,8> --timeout-ms <ms>\n' +
		'  Gates: --require-all --enforce-budgets --accept-projection-memory\n' +
		'  Queries: --game-symbol-query <text> --workspace-symbol-query <text> --text-query <text> --broad-text-query <text>\n' +
		'           --regex-query <pattern> --wiki-query <text> --relationship-method-query <text>\n' +
		'  Identity: --commit <sha>\n' +
		'  Output: --json-out <path> --markdown-out <path>\n' +
		'          --baseline-report <json> --paired-baseline-server <executable>\n' +
		'          --comparison-json-out <path> --comparison-markdown-out <path>\n',
	);
	process.exit(error ? 2 : 0);
}

const options = parseArguments(process.argv.slice(2));
const report = await runReport(options);
report.paired = await measurePairedRuns(options);
writeReport(options.jsonOut, `${JSON.stringify(report, null, 2)}\n`);
writeReport(options.markdownOut, renderMarkdown(report));
if (options.baselineReport) {
	const baseline = JSON.parse(readFileSync(resolve(options.baselineReport), 'utf8'));
	const comparison = compareReports(baseline, report);
	const jsonPath = options.comparisonJsonOut ?? 'tools/reports/mcp-runtime-performance.comparison.json';
	const markdownPath = options.comparisonMarkdownOut ?? 'tools/reports/mcp-runtime-performance.comparison.md';
	writeReport(jsonPath, `${JSON.stringify(comparison, null, 2)}\n`);
	writeReport(markdownPath, renderComparisonMarkdown(comparison));
}
process.stdout.write(
	`MCP runtime report: ${report.verdict}; ${report.coverage.exercised}/${report.coverage.listed} non-Workbench tools exercised.\n` +
	`JSON: ${options.jsonOut}\nMarkdown: ${options.markdownOut}\n`,
);
if (report.coverage.failed > 0 || (options.requireAll && report.coverage.exercised !== report.coverage.listed)) {
	process.exitCode = 1;
}
