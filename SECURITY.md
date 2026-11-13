# Security policy

Delux runs commands on your computers over SSH and renders output from remote tmux sessions in your terminal, so bugs in command construction, host-key handling, or terminal output sanitization can be security issues.

## Reporting a vulnerability

Email [security@delux.sh](mailto:security@delux.sh), or use **Report a vulnerability** on the repository's **Security** tab when available. Please don't open a public issue or pull request for it.

Include the Delux version (`delux --version`), your OS, and a minimal reproduction, such as a session name, hostname, or remote output that triggers the problem. Remove credentials and unrelated private data; never send passwords, private keys, or tokens. We aim to acknowledge reports within a week.

## Supported versions

Before the first release, fixes land on `main`. After publication, security fixes target the latest release; older versions do not have a separate maintenance branch.
