import { EventEmitter } from "node:events";

export interface ShareableNode {
    id: number;
    displayName: string;
    applicationName: string | null;
    nodeName: string | null;
    description: string | null;
    mediaName: string | null;
    binary: string | null;
    processId: number | null;
    isDevice: boolean;
    mediaClass: string | null;
    isVirtual: boolean;
}

export interface RouteFilter {
    onlySpeakers?: boolean;
    onlyDefaultSpeakers?: boolean;
    ignoreDevices?: boolean;
    ignoreVirtual?: boolean;
    ignoreInputMedia?: boolean;
}

/**
 * A best-effort hint correlating an in-progress KDE/KWin window-share
 * (portal ScreenCast session) with a likely audio-producing app, derived
 * from KWin's own PipeWire video node naming convention. See the Rust
 * `models::ScreencastHint` doc comment for the full mechanism. `null`
 * (from `findScreencastHint()`) is the normal case when no window share
 * is active or the compositor isn't KWin -- callers should fall back to
 * showing the full unfiltered node list, not treat it as an error.
 */
export interface ScreencastHint {
    desktopFileId: string;
    hint: string;
}

export interface VirtualSinkInfo {
    sinkName: string;
    monitorSource: string;
    nodeId: number;
    virtualMicName?: string | null;
    virtualMicDescription?: string | null;
}

export interface AudioSharePatchbayOptions {
    command: string;
    args?: readonly string[];
    cwd?: string;
    env?: NodeJS.ProcessEnv;
    requestTimeoutMs?: number;
    shutdownTimeoutMs?: number;
    sinkPrefix?: string;
    sinkDescription?: string;
    virtualMic?: boolean;
    /**
     * Creates the virtual sink as a plain Audio/Sink (rather than
     * Audio/Sink/Virtual) and makes it the system default sink once ready,
     * so app-routed audio actually reaches Discord's own "Stream With
     * Audio" capture (which always grabs the *default* sink's monitor).
     * Mutually exclusive with virtualMic -- see PatchbayConfig's Rust doc
     * comment for why.
     */
    sinkBecomesDefault?: boolean;
    virtualMicName?: string;
    virtualMicDescription?: string;
}

export declare class AudioSharePatchbay extends EventEmitter {
    constructor(options: AudioSharePatchbayOptions);
    hasPipeWire(): Promise<boolean>;
    listShareableNodes(includeDevices?: boolean): Promise<ShareableNode[]>;
    findScreencastHint(): Promise<ScreencastHint | null>;
    ensureVirtualSink(): Promise<VirtualSinkInfo>;
    routeNodes(nodeIds: number[], filter?: RouteFilter): Promise<VirtualSinkInfo>;
    clearRoutes(): Promise<void>;
    setVirtualMicMute(mute: boolean): Promise<void>;
    setDefaultSinkToVirtual(): Promise<void>;
    restoreDefaultSink(): Promise<void>;
    /**
     * Routes the given node id(s) directly into every one of Discord's
     * own `discord_capture` screenshare-audio nodes, replacing whatever
     * selection was previously routed there. Pass an empty array to stop
     * routing anything. Requires `discord-capture-shim` to be
     * `LD_PRELOAD`'d into the Discord process, or `discord_capture`
     * nodes will keep auto-linking themselves to every detected app
     * regardless of this call. `filter` behaves identically to
     * `routeNodes`'s (applied server-side via the same `should_link`
     * decision). Unlike `routeNodes`, needs no prior
     * `ensureVirtualSink()` call -- `discord_capture` is Discord's own
     * node, read directly in-process, so there's no virtual sink/mic
     * involved on this path.
     */
    setDiscordCaptureTargets(nodeIds: number[], filter?: RouteFilter): Promise<void>;
    dispose(): Promise<void>;

    on(eventName: 'graphChanged' | 'monitorDied', listener: () => void): this;
    once(eventName: 'graphChanged' | 'monitorDied', listener: () => void): this;
    off(eventName: 'graphChanged' | 'monitorDied', listener: () => void): this;
    emit(eventName: 'graphChanged' | 'monitorDied'): boolean;

    on(eventName: string | symbol, listener: (...args: any[]) => void): this;
    once(eventName: string | symbol, listener: (...args: any[]) => void): this;
    off(eventName: string | symbol, listener: (...args: any[]) => void): this;
    emit(eventName: string | symbol, ...args: any[]): boolean;
}

export declare function hasPipeWire(
    options: AudioSharePatchbayOptions,
): Promise<boolean>;
