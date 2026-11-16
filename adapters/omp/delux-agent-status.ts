// Reports an OMP session's status to tmux, where Delux reads it.
//
// Install on each computer that runs OMP inside tmux:
//   cp delux-agent-status.ts ~/.omp/agent/extensions/
//
// It sets the pane option `@agent_status` to "working", "waiting" or "done",
// followed by the Unix time of the change. Outside tmux it does nothing.

import { spawn } from "node:child_process";

import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";

type State = "working" | "waiting" | "done";

/** The only event and context fields this adapter reads. */
interface StatusEvent {
	toolName?: string;
	toolCallId?: string;
}
interface StatusContext {
	agent?: { kind?: string };
}
type Handler = (event: StatusEvent | undefined, ctx: StatusContext | undefined) => Promise<void> | void;

// Two tmux commands started together can finish in either order, so each
// change waits for the one before it. Otherwise "done" could lose to "working".
let applied: Promise<void> = Promise.resolve();

/** Set, or with `null` clear, the status on this process's own tmux pane. */
function report(state: State | null): Promise<void> {
	const pane = process.env.TMUX_PANE;
	if (!process.env.TMUX || !pane) return applied;
	const args =
		state === null
			? ["set-option", "-p", "-u", "-t", pane, "@agent_status"]
			: ["set-option", "-p", "-t", pane, "@agent_status", `${state} ${Math.floor(Date.now() / 1000)}`];
	applied = applied.then(() => {
		const { promise, resolve } = Promise.withResolvers<void>();
		try {
			// A stuck tmux must not hold up the agent, and a missing one is not an error.
			const child = spawn("tmux", args, { stdio: "ignore", timeout: 2000 });
			child.on("error", () => resolve());
			child.on("exit", () => resolve());
		} catch {
			resolve();
		}
		return promise;
	});
	return applied;
}

export default function deluxAgentStatus(pi: ExtensionAPI) {
	// OMP types each event's payload separately and differs between versions;
	// this adapter reads two optional fields, so it registers through one shape.
	const on = pi.on.bind(pi) as unknown as (event: string, handler: Handler) => void;

	let running = false;
	// Open requests that need the user: approvals, plan reviews, questions.
	const waiting = new Set<string>();

	/**
	 * Registers a handler for the main agent only. The factory is bound again
	 * for every subagent, and a finished subagent must not mark the session done.
	 */
	function onMain(event: string, change: (event: StatusEvent | undefined) => State | null | undefined): void {
		on(event, (payload, ctx) => {
			if (ctx?.agent?.kind === "sub") return;
			const state = change(payload);
			// Returned so a shutdown waits for the status to be cleared.
			if (state !== undefined) return report(state);
		});
	}
	const current = (): State => (waiting.size > 0 ? "waiting" : running ? "working" : "done");

	onMain("session_start", () => {
		running = false;
		waiting.clear();
		return null;
	});
	onMain("agent_start", () => {
		running = true;
		return current();
	});
	// Refresh the timestamp during long runs so "working" is not taken for stale.
	onMain("turn_start", () => (running ? current() : undefined));
	onMain("agent_end", () => {
		running = false;
		waiting.clear();
		return current();
	});

	const requests: [opened: string, closed: string, key: (event: StatusEvent | undefined) => string | undefined][] = [
		["tool_approval_requested", "tool_approval_resolved", () => "approval"],
		["plan_review_requested", "plan_review_resolved", () => "plan"],
		["tool_call", "tool_result", event => (event?.toolName === "ask" ? `ask:${event.toolCallId}` : undefined)],
	];
	for (const [opened, closed, key] of requests) {
		onMain(opened, event => {
			const id = key(event);
			if (id === undefined) return undefined;
			waiting.add(id);
			return current();
		});
		onMain(closed, event => {
			const id = key(event);
			return id !== undefined && waiting.delete(id) ? current() : undefined;
		});
	}

	onMain("session_shutdown", () => null);
}
