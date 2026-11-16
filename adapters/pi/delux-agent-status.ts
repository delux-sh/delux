// delux-agent-status adapter. Installed by `delux agents install`; reinstalling replaces this file.
// Reports a Pi or OMP session's status to tmux, where Delux reads it.
//
// Install on each computer that runs Pi or OMP inside tmux:
//   delux agents install pi
//   delux agents install omp
//
// It sets the pane option `@agent_status` to "working", "waiting" or "done",
// followed by the Unix time of the change. Outside tmux it does nothing.
//
// One file serves both tools: OMP is built on Pi and can share Pi's extension
// directory, where two different adapters would both load and disagree.

import { spawn } from "node:child_process";

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
// Structural on purpose: each tool types its events differently and adds
// events between versions, and importing either tool's types would tie this
// file to one of them.
interface Host {
	on(event: string, handler: Handler): void;
}

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
	applied = applied.then(
		() =>
			new Promise<void>(resolve => {
				try {
					// A stuck tmux must not hold up the agent, and a missing one is not an error.
					const child = spawn("tmux", args, { stdio: "ignore", timeout: 2000 });
					child.on("error", () => resolve());
					child.on("exit", () => resolve());
				} catch {
					resolve();
				}
			}),
	);
	return applied;
}

export default function deluxAgentStatus(pi: Host) {
	const on = pi.on.bind(pi);

	let running = false;
	// Open requests that need the user: approvals, plan reviews, questions.
	const requests = new Set<string>();
	// Blocking prompts shown by the tool or by another extension.
	let prompts = 0;

	/**
	 * Registers a handler for the main agent only. OMP binds the factory again
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
	const current = (): State => (requests.size > 0 || prompts > 0 ? "waiting" : running ? "working" : "done");

	onMain("session_start", () => {
		running = false;
		requests.clear();
		prompts = 0;
		return null;
	});
	onMain("agent_start", () => {
		running = true;
		return current();
	});
	// Refresh the timestamp during long runs so "working" is not taken for stale.
	onMain("turn_start", () => (running ? current() : undefined));
	// A run can end and the tool still continue on its own (retry, compaction,
	// a queued message). `agent_start` then reports "working" again, and Pi's
	// `agent_settled` confirms the final stop.
	for (const event of ["agent_end", "agent_settled"]) {
		onMain(event, () => {
			running = false;
			requests.clear();
			prompts = 0;
			return current();
		});
	}

	// OMP: approvals, plan reviews, and the question tool.
	const pairs: [opened: string, closed: string, key: (event: StatusEvent | undefined) => string | undefined][] = [
		["tool_approval_requested", "tool_approval_resolved", () => "approval"],
		["plan_review_requested", "plan_review_resolved", () => "plan"],
		["tool_call", "tool_result", event => (event?.toolName === "ask" ? `ask:${event.toolCallId}` : undefined)],
	];
	for (const [opened, closed, key] of pairs) {
		onMain(opened, event => {
			const id = key(event);
			if (id === undefined) return undefined;
			requests.add(id);
			return current();
		});
		onMain(closed, event => {
			const id = key(event);
			return id !== undefined && requests.delete(id) ? current() : undefined;
		});
	}

	// Pi: every blocking prompt, including ones other extensions show.
	onMain("ui_prompt_start", () => {
		prompts += 1;
		return current();
	});
	onMain("ui_prompt_end", () => {
		prompts = Math.max(0, prompts - 1);
		return current();
	});

	onMain("session_shutdown", () => null);
}
