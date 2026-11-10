export type ShortcutDispatchSource = 'register-hotkey' | 'native-hook';

interface AcceptedDispatch {
    source: ShortcutDispatchSource;
    timestampMs: number;
}

/**
 * Serializes the RegisterHotKey and WH_KEYBOARD_LL delivery paths.
 *
 * Both callbacks ultimately arrive on Electron's main event loop, so this is
 * the single owner of dispatch ordering. A short cross-source window removes
 * the second delivery of one physical actuation without suppressing normal OS
 * key-repeat (which arrives repeatedly from the same source).
 */
export class ShortcutDispatchBarrier {
    private readonly lastAcceptedByAction = new Map<string, AcceptedDispatch>();

    public constructor(
        private readonly duplicateWindowMs = 100,
        private readonly monotonicNow: () => number = () => performance.now(),
    ) {
        if (!Number.isFinite(duplicateWindowMs) || duplicateWindowMs < 0) {
            throw new RangeError('duplicateWindowMs must be a finite non-negative number');
        }
    }

    public accept(actionId: string, source: ShortcutDispatchSource): boolean {
        const now = this.monotonicNow();
        const previous = this.lastAcceptedByAction.get(actionId);

        if (previous) {
            const elapsedMs = now - previous.timestampMs;
            if (
                previous.source !== source
                && elapsedMs >= 0
                && elapsedMs < this.duplicateWindowMs
            ) {
                return false;
            }
        }

        // A negative elapsed time means the supplied monotonic clock was reset
        // (for example in a test or after an unusual runtime transition). Treat
        // it as a fresh epoch rather than suppressing shortcuts indefinitely.
        this.lastAcceptedByAction.set(actionId, { source, timestampMs: now });
        return true;
    }

    public clear(): void {
        this.lastAcceptedByAction.clear();
    }
}
