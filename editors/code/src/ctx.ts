import * as vscode from "vscode";
import * as lc from "vscode-languageclient/node";
import * as ra from "./lsp_ext";

import { Config, prepareVSCodeConfig } from "./config";
import { createClient } from "./client";
import {
    isDocumentInWorkspace,
    isRustDocument,
    isRustEditor,
    LazyOutputChannel,
    log,
    type RustEditor,
} from "./util";
import type { ServerStatusParams } from "./lsp_ext";
import {
    type Dependency,
    type DependencyFile,
    RustDependenciesProvider,
    type DependencyId,
} from "./dependencies_provider";
import { execRevealDependency } from "./commands";
import { PersistentState } from "./persistent_state";
import { bootstrap, getVerus, getVerusVersion, validRustToolchain} from "./bootstrap";
import type { RustAnalyzerExtensionApi } from "./main";
import type { JsonProject } from "./rust_project";
import { prepareTestExplorer } from "./test_explorer";
import { spawn } from "node:child_process";
import { text } from "node:stream/consumers";

// We only support local folders, not eg. Live Share (`vlsl:` scheme), so don't activate if
// only those are in use. We use "Empty" to represent these scenarios
// (r-a still somewhat works with Live Share, because commands are tunneled to the host)

export type Workspace =
    | { kind: "Empty" }
    | {
          kind: "Workspace Folder";
      }
    | {
          kind: "Detached Files";
          files: vscode.TextDocument[];
      };

export function fetchWorkspace(): Workspace {
    const folders = (vscode.workspace.workspaceFolders || []).filter(
        (folder) => folder.uri.scheme === "file",
    );
    const rustDocuments = vscode.workspace.textDocuments.filter((document) =>
        isRustDocument(document),
    );

    return folders.length === 0
        ? rustDocuments.length === 0
            ? { kind: "Empty" }
            : {
                  kind: "Detached Files",
                  files: rustDocuments,
              }
        : { kind: "Workspace Folder" };
}

export type CommandFactory = {
    enabled: (ctx: CtxInit) => Cmd;
    disabled?: (ctx: Ctx) => Cmd;
};

export type CtxInit = Ctx & {
    readonly client: lc.LanguageClient;
};

export class Ctx implements RustAnalyzerExtensionApi {
    readonly statusBar: vscode.StatusBarItem;
    config: Config;
    readonly workspace: Workspace;
    readonly version: string;

    private _client: lc.LanguageClient | undefined;
    private _serverPath: string | undefined;
    private traceOutputChannel: vscode.OutputChannel | undefined;
    private testController: vscode.TestController | undefined;
    private outputChannel: vscode.OutputChannel | undefined;
    private clientSubscriptions: Disposable[];
    private state: PersistentState;
    private commandFactories: Record<string, CommandFactory>;
    private commandDisposables: Disposable[];
    private unlinkedFiles: vscode.Uri[];
    private _dependencies: RustDependenciesProvider | undefined;
    private _treeView: vscode.TreeView<Dependency | DependencyFile | DependencyId> | undefined;
    private lastStatus: ServerStatusParams | { health: "stopped" } = { health: "stopped" };
    private _serverVersion: string;
    private _verusVersion: string;

    get serverPath(): string | undefined {
        return this._serverPath;
    }

    get serverVersion(): string | undefined {
        return this._serverVersion;
    }

    get verusVersion(): string {
        return this._verusVersion;
    }

    get client() {
        return this._client;
    }

    get treeView() {
        return this._treeView;
    }

    get dependencies() {
        return this._dependencies;
    }

    constructor(
        readonly extCtx: vscode.ExtensionContext,
        commandFactories: Record<string, CommandFactory>,
        workspace: Workspace,
    ) {
        extCtx.subscriptions.push(this);
        this.version = extCtx.extension.packageJSON.version ?? "<unknown>";
        this._serverVersion = "<not running>";
        this._verusVersion = "unknown";
        this.config = new Config(extCtx);
        this.statusBar = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left);
        if (this.config.testExplorer) {
            this.testController = vscode.tests.createTestController(
                "rustAnalyzerTestController",
                "Verus Analyzer test controller",
            );
        }
        this.workspace = workspace;
        this.clientSubscriptions = [];
        this.commandDisposables = [];
        this.commandFactories = commandFactories;
        this.unlinkedFiles = [];
        this.state = new PersistentState(extCtx.globalState);

        this.updateCommands("disable");
        this.setServerStatus({
            health: "stopped",
        });
    }

    dispose() {
        this.config.dispose();
        this.statusBar.dispose();
        this.testController?.dispose();
        void this.disposeClient();
        this.commandDisposables.forEach((disposable) => disposable.dispose());
    }

    async onWorkspaceFolderChanges() {
        const workspace = fetchWorkspace();
        if (workspace.kind === "Detached Files" && this.workspace.kind === "Detached Files") {
            if (workspace.files !== this.workspace.files) {
                if (this.client?.isRunning()) {
                    // Ideally we wouldn't need to tear down the server here, but currently detached files
                    // are only specified at server start
                    await this.stopAndDispose();
                    await this.start();
                }
                return;
            }
        }
        if (workspace.kind === "Workspace Folder" && this.workspace.kind === "Workspace Folder") {
            return;
        }
        if (workspace.kind === "Empty") {
            await this.stopAndDispose();
            return;
        }
        if (this.client?.isRunning()) {
            await this.restart();
        }
    }

    private async getOrCreateClient() {
        if (this.workspace.kind === "Empty") {
            return;
        }

        if (!this.traceOutputChannel) {
            this.traceOutputChannel = new LazyOutputChannel("Verus Analyzer Language Server Trace");
            this.pushExtCleanup(this.traceOutputChannel);
        }
        if (!this.outputChannel) {
            this.outputChannel = vscode.window.createOutputChannel("Verus Analyzer Language Server");
            this.pushExtCleanup(this.outputChannel);
        }

        if (!this._client) {
            this._serverPath = await bootstrap(this.extCtx, this.config, this.state).catch(
                (err) => {
                    let message = "bootstrap error. ";

                    message +=
                        'See the logs in "OUTPUT > Verus Analyzer Client" (should open automatically). ';
                    message +=
                        'To enable verbose logs use { "verus-analyzer.trace.extension": true }';

                    log.error("Bootstrap error", err);
                    throw new Error(message);
                },
            );
            text(spawn(this._serverPath, ["--version"]).stdout.setEncoding("utf-8")).then(
                (data) => {
                    const prefix = `verus-analyzer `;
                    this._serverVersion = data
                        .slice(data.startsWith(prefix) ? prefix.length : 0)
                        .trim();
                    this.refreshServerStatus();
                },
                (_) => {
                    this._serverVersion = "<unknown>";
                    this.refreshServerStatus();
                },
            );
            const haveValidRustToolchain:Boolean = await validRustToolchain();
            if (!haveValidRustToolchain) {
                log.info("Failed to find rustup");
                return;
            }
            const verusPath = await getVerus(this.extCtx, this.config);
            log.info("Using verus binary at", verusPath);
            process.env['VERUS_BINARY_PATH'] = verusPath;
            this._verusVersion = await getVerusVersion(verusPath);
            const newEnv = Object.assign({}, process.env, this.config.serverExtraEnv);
            const run: lc.Executable = {
                command: this._serverPath,
                options: { env: newEnv },
            };
            const serverOptions = {
                run,
                debug: run,
            };

            let rawInitializationOptions = vscode.workspace.getConfiguration("verus-analyzer");
            if (this.config.discoverProjectRunner) {
                const command = `${this.config.discoverProjectRunner}.discoverWorkspaceCommand`;
                log.info(`running command: ${command}`);
                const uris = vscode.workspace.textDocuments
                    .filter(isRustDocument)
                    .map((document) => document.uri);
                const projects: JsonProject[] = await vscode.commands.executeCommand(command, uris);
                this.setWorkspaces(projects);
            }

            if (this.workspace.kind === "Detached Files") {
                rawInitializationOptions = {
                    detachedFiles: this.workspace.files.map((file) => file.uri.fsPath),
                    ...rawInitializationOptions,
                };
            }

            const initializationOptions = prepareVSCodeConfig(
                rawInitializationOptions,
                (key, obj) => {
                    // we only want to set discovered workspaces on the right key
                    // and if a workspace has been discovered.
                    if (key === "linkedProjects" && this.config.discoveredWorkspaces.length > 0) {
                        obj["linkedProjects"] = this.config.discoveredWorkspaces;
                    }
                },
            );

            this._client = await createClient(
                this.traceOutputChannel,
                this.outputChannel,
                initializationOptions,
                serverOptions,
                this.config,
                this.unlinkedFiles,
            );
            this.pushClientCleanup(
                this._client.onNotification(ra.serverStatus, (params) =>
                    this.setServerStatus(params),
                ),
            );
            this.pushClientCleanup(
                this._client.onNotification(ra.openServerLogs, () => {
                    this.outputChannel!.show();
                }),
            );
            this.pushClientCleanup(
                this._client.onNotification(ra.unindexedProject, async (params) => {
                    if (this.config.discoverProjectRunner) {
                        const command = `${this.config.discoverProjectRunner}.discoverWorkspaceCommand`;
                        log.info(`running command: ${command}`);
                        const uris = params.textDocuments.map((doc) =>
                            vscode.Uri.parse(doc.uri, true),
                        );
                        const projects: JsonProject[] = await vscode.commands.executeCommand(
                            command,
                            uris,
                        );
                        this.setWorkspaces(projects);
                        await this.notifyRustAnalyzer();
                    }
                }),
            );
        }
        return this._client;
    }

    async start() {
        log.info("Starting language client");
        const client = await this.getOrCreateClient();
        if (!client) {
            return;
        }
        await client.start();
        this.updateCommands();

        if (this.testController) {
            prepareTestExplorer(this, this.testController, client);
        }
        if (this.config.showDependenciesExplorer) {
            this.prepareTreeDependenciesView(client);
        }
    }

    private prepareTreeDependenciesView(client: lc.LanguageClient) {
        const ctxInit: CtxInit = {
            ...this,
            client: client,
        };
        this._dependencies = new RustDependenciesProvider(ctxInit);
        this._treeView = vscode.window.createTreeView("verusDependencies", {
            treeDataProvider: this._dependencies,
            showCollapseAll: true,
        });

        this.pushExtCleanup(this._treeView);
        vscode.window.onDidChangeActiveTextEditor(async (e) => {
            // we should skip documents that belong to the current workspace
            if (this.shouldRevealDependency(e)) {
                try {
                    await execRevealDependency(e);
                } catch (reason) {
                    await vscode.window.showErrorMessage(`Dependency error: ${reason}`);
                }
            }
        });

        this.treeView?.onDidChangeVisibility(async (e) => {
            if (e.visible) {
                const activeEditor = vscode.window.activeTextEditor;
                if (this.shouldRevealDependency(activeEditor)) {
                    try {
                        await execRevealDependency(activeEditor);
                    } catch (reason) {
                        await vscode.window.showErrorMessage(`Dependency error: ${reason}`);
                    }
                }
            }
        });
    }

    private shouldRevealDependency(e: vscode.TextEditor | undefined): e is RustEditor {
        return (
            e !== undefined &&
            isRustEditor(e) &&
            !isDocumentInWorkspace(e.document) &&
            (this.treeView?.visible || false)
        );
    }

    async restart() {
        // FIXME: We should re-use the client, that is ctx.deactivate() if none of the configs have changed
        await this.stopAndDispose();
        await this.start();
    }

    async stop() {
        if (!this._client) {
            return;
        }
        log.info("Stopping language client");
        this.updateCommands("disable");
        await this._client.stop();
    }

    async stopAndDispose() {
        if (!this._client) {
            return;
        }
        log.info("Disposing language client");
        this.updateCommands("disable");
        await this.disposeClient();
    }

    private async disposeClient() {
        this.clientSubscriptions?.forEach((disposable) => disposable.dispose());
        this.clientSubscriptions = [];
        await this._client?.dispose();
        this._serverPath = undefined;
        this._client = undefined;
    }

    get activeRustEditor(): RustEditor | undefined {
        const editor = vscode.window.activeTextEditor;
        return editor && isRustEditor(editor) ? editor : undefined;
    }

    get extensionPath(): string {
        return this.extCtx.extensionPath;
    }

    get subscriptions(): Disposable[] {
        return this.extCtx.subscriptions;
    }

    setWorkspaces(workspaces: JsonProject[]) {
        this.config.discoveredWorkspaces = workspaces;
    }

    async notifyRustAnalyzer(): Promise<void> {
        // this is a workaround to avoid needing writing the `rust-project.json` into
        // a workspace-level VS Code-specific settings folder. We'd like to keep the
        // `rust-project.json` entirely in-memory.
        await this.client?.sendNotification(lc.DidChangeConfigurationNotification.type, {
            settings: "",
        });
    }

    private updateCommands(forceDisable?: "disable") {
        this.commandDisposables.forEach((disposable) => disposable.dispose());
        this.commandDisposables = [];

        const clientRunning = (!forceDisable && this._client?.isRunning()) ?? false;
        const isClientRunning = function (_ctx: Ctx): _ctx is CtxInit {
            return clientRunning;
        };

        for (const [name, factory] of Object.entries(this.commandFactories)) {
            const fullName = `verus-analyzer.${name}`;
            let callback;
            if (isClientRunning(this)) {
                // we asserted that `client` is defined
                callback = factory.enabled(this);
            } else if (factory.disabled) {
                callback = factory.disabled(this);
            } else {
                callback = () =>
                    vscode.window.showErrorMessage(
                        `command ${fullName} failed: verus-analyzer server is not running`,
                    );
            }

            this.commandDisposables.push(vscode.commands.registerCommand(fullName, callback));
        }
    }

    setServerStatus(status: ServerStatusParams | { health: "stopped" }) {
        this.lastStatus = status;
        this.updateStatusBarItem();
    }
    refreshServerStatus() {
        this.updateStatusBarItem();
    }
    private updateStatusBarItem() {
        let icon = "";
        const status = this.lastStatus;
        const statusBar = this.statusBar;
        statusBar.show();
        statusBar.tooltip = new vscode.MarkdownString("", true);
        statusBar.tooltip.isTrusted = true;
        switch (status.health) {
            case "ok":
                statusBar.color = undefined;
                statusBar.backgroundColor = undefined;
                if (this.config.statusBarClickAction === "stopServer") {
                    statusBar.command = "verus-analyzer.stopServer";
                } else {
                    statusBar.command = "verus-analyzer.openLogs";
                }
                this.dependencies?.refresh();
                break;
            case "warning":
                statusBar.color = new vscode.ThemeColor("statusBarItem.warningForeground");
                statusBar.backgroundColor = new vscode.ThemeColor(
                    "statusBarItem.warningBackground",
                );
                statusBar.command = "verus-analyzer.openLogs";
                icon = "$(warning) ";
                break;
            case "error":
                statusBar.color = new vscode.ThemeColor("statusBarItem.errorForeground");
                statusBar.backgroundColor = new vscode.ThemeColor("statusBarItem.errorBackground");
                statusBar.command = "verus-analyzer.openLogs";
                icon = "$(error) ";
                break;
            case "stopped":
                statusBar.tooltip.appendText("Server is stopped");
                statusBar.tooltip.appendMarkdown(
                    "\n\n[Start server](command:verus-analyzer.startServer)",
                );
                statusBar.color = new vscode.ThemeColor("statusBarItem.warningForeground");
                statusBar.backgroundColor = new vscode.ThemeColor(
                    "statusBarItem.warningBackground",
                );
                statusBar.command = "verus-analyzer.startServer";
                statusBar.text = "$(stop-circle) verus-analyzer";
                return;
        }
        if (status.message) {
            statusBar.tooltip.appendText(status.message);
        }
        if (statusBar.tooltip.value) {
            statusBar.tooltip.appendMarkdown("\n\n---\n\n");
        }

        const toggleCheckOnSave = this.config.checkOnSave ? "Disable" : "Enable";
        statusBar.tooltip.appendMarkdown(
            `[Extension Info](command:analyzer.serverVersion "Show version and server binary info"): Version ${this.version}, Server Version ${this._serverVersion}` +
                `, Verus Version ${this._verusVersion}` +
                "\n\n---\n\n" +
                '[$(terminal) Open Logs](command:verus-analyzer.openLogs "Open the server logs")' +
                "\n\n" +
                `[$(settings) ${toggleCheckOnSave} Check on Save](command:verus-analyzer.toggleCheckOnSave "Temporarily ${toggleCheckOnSave.toLowerCase()} check on save functionality")` +
                "\n\n" +
                '[$(refresh) Reload Workspace](command:verus-analyzer.reloadWorkspace "Reload and rediscover workspaces")' +
                "\n\n" +
                '[$(symbol-property) Rebuild Build Dependencies](command:verus-analyzer.rebuildProcMacros "Rebuild build scripts and proc-macros")' +
                "\n\n" +
                '[$(stop-circle) Stop server](command:verus-analyzer.stopServer "Stop the server")' +
                "\n\n" +
                '[$(debug-restart) Restart server](command:verus-analyzer.restartServer "Restart the server")',
        );
        if (!status.quiescent) icon = "$(loading~spin) ";
        statusBar.text = `${icon}verus-analyzer`;
    }

    pushExtCleanup(d: Disposable) {
        this.extCtx.subscriptions.push(d);
    }

    pushClientCleanup(d: Disposable) {
        this.clientSubscriptions.push(d);
    }
}

export interface Disposable {
    dispose(): void;
}

export type Cmd = (...args: any[]) => unknown;
