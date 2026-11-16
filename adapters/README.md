# Agent status adapters

Delux shows a session's agent status when something in its tmux pane sets the `@agent_status` option. Delux itself knows nothing about any agent tool; an adapter is the small piece that makes one tool report its state.

## The contract

```sh
tmux set-option -p -t "$TMUX_PANE" @agent_status "waiting $(date +%s)"
```

- The value is `working`, `waiting` or `done`, optionally followed by a space and the Unix time it was set. Anything else is ignored.
- `waiting` means the agent needs you: a question, an approval, a review.
- Clear it with `tmux set-option -p -u -t "$TMUX_PANE" @agent_status`.
- Set it on the pane the agent runs in. A session shows the most urgent status among its panes: waiting, then working, then done.
- Include the time. Delux hides `working` once neither the status nor the pane's output has changed for ten minutes, so a crashed agent does not look busy forever.

Any tool that can run a command on its own events can follow this contract.

## OMP

`omp/delux-agent-status.ts` is an OMP extension. Install it on each computer where OMP runs inside tmux:

```sh
cp adapters/omp/delux-agent-status.ts ~/.omp/agent/extensions/
```

New OMP sessions pick it up. It reports `working` while the main agent runs, `waiting` while an approval, plan review or question is open, and `done` when the run ends; it clears the status when the session starts and when it shuts down. Subagents do not change the status. Outside tmux it does nothing.
