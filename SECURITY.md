# Security Policy

## Supported Versions

vc-frame is developed as a rolling release. Security fixes are applied to the
latest published version on the `main` branch.

| Version | Supported          |
| ------- | ------------------ |
| 0.45.x  | :white_check_mark: |
| < 0.45  | :x:                |

Older releases are not maintained. If you are running an older version, please
upgrade before reporting a security issue.

## Reporting a Vulnerability

Please report security vulnerabilities **privately** — do not open a public
issue, pull request, or discussion for a suspected vulnerability.

Email **hello@vetcoders.io** with:

- a description of the vulnerability and its impact,
- the version or commit affected,
- steps to reproduce (a minimal proof of concept if possible).

If GitHub private vulnerability reporting is enabled for this repository, you may
instead open a private advisory from the repository's **Security** tab
("Report a vulnerability").

We will acknowledge your report, keep you informed about remediation, and credit
you in the release notes once a fix ships — unless you prefer to remain
anonymous. Please give us a reasonable window to investigate and release a fix
before any public disclosure.

## Scope

vc-frame is a terminal workspace and multiplexer. In-scope issues include, but
are not limited to: privilege escalation, arbitrary code execution via
configuration, layouts, or plugins, escape from the plugin WebAssembly sandbox,
and exposure of session sockets or authentication tokens.
