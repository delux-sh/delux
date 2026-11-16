// delux-agent-status adapter. Installed by `delux agents install`; reinstalling replaces this file.
// Reports an OpenCode session's status to tmux, where Delux reads it.
//
// Install on each computer that runs OpenCode inside tmux:
//   delux agents install opencode
//
// It sets the pane option `@agent_status` to "working", "waiting" or "done",
// followed by the Unix time of the change. Outside tmux it does nothing.

import { spawn } from "node:child_process";

// Two tmux commands started together can finish in either order, so each
// change waits for the one before it. Otherwise "done" could lose to "working".
let applied = Promise.resolve();

/** Set, or with `null` clear, the status on this process's own tmux pane. */
function report(state) {
	const pane = process.env.TMUX_PANE;
	if (!process.env.TMUX || !pane) return applied;
	const args =
		state === null
			? ["set-option", "-p", "-u", "-t", pane, "@agent_status"]
			: ["set-option", "-p", "-t", pane, "@agent_status", `${state} ${Math.floor(Date.now() / 1000)}`];
	applied = applied.then(
		() =>
			new Promise(resolve => {
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

export const DeluxAgentStatus = async () => {
	// Sessions started by another session (sub-agents). Their runs belong to
	// the parent, so a finished sub-agent must not mark the pane done.
	const children = new Set();
	// Top-level sessions with a run in progress.
	const busy = new Set();
	// Open requests that need the user: permissions and questions, by session.
	const waiting = new Map();
	let started = false;

	const current = () => (waiting.size > 0 ? "waiting" : busy.size > 0 ? "working" : "done");
	const update = () => {
		// Nothing has run yet: an idle event at startup is not a finished task.
		if (!started) return;
		return report(current());
	};
	const open = (session, id) => {
		const ids = waiting.get(session) ?? new Set();
		ids.add(id);
		waiting.set(session, ids);
		started = true;
	};
	const close = (session, id) => {
		const ids = waiting.get(session);
		if (!ids) return;
		// Event shapes differ between versions; an answer whose id is unknown
		// still means this session's request was answered.
		if (!ids.delete(id)) ids.clear();
		if (ids.size === 0) waiting.delete(session);
	};

	await report(null);

	return {
		event: async ({ event }) => {
			const properties = event?.properties ?? {};
			const session = properties.sessionID ?? properties.info?.id;
			switch (event?.type) {
				case "session.created":
					if (properties.info?.parentID) children.add(properties.info.id);
					return;
				case "session.deleted":
					children.delete(session);
					busy.delete(session);
					waiting.delete(session);
					return update();
				case "session.status":
					if (children.has(session)) return;
					if (properties.status?.type === "idle") busy.delete(session);
					else {
						busy.add(session);
						started = true;
					}
					return update();
				case "session.idle":
					if (children.has(session)) return;
					busy.delete(session);
					return update();
				case "permission.asked":
				case "permission.updated":
				case "question.asked":
					open(session, properties.id ?? properties.requestID ?? properties.permissionID);
					return update();
				case "permission.replied":
				case "question.replied":
				case "question.rejected":
					close(session, properties.permissionID ?? properties.requestID ?? properties.id);
					return update();
			}
		},
		dispose: async () => {
			await report(null);
		},
	};
};
