# Security Policy

## Supported versions

Security fixes are made on the latest release. Please upgrade to the newest
version of wsblast before reporting a problem.

| Version | Supported |
| :--- | :--- |
| Latest release | Yes |
| Older releases | No |

## Reporting a vulnerability

Please do **not** open a public issue, pull request or discussion for a
security problem.

Report it privately through GitHub's security advisories instead:

1. Go to <https://github.com/jephter-olamiposi/wsblast/security/advisories/new>
   (the **Security** tab, then **Report a vulnerability**).
2. Describe the issue, the affected version (`wsblast --version`), your
   platform, and the steps or input needed to reproduce it. A proof of concept
   against a local server, such as `examples/echo_server.rs`, is ideal.

You should get an acknowledgement within a few days. Once the issue is
confirmed, a fix is prepared in a private fork, a patched release is
published, and the advisory is made public with credit to the reporter unless
you prefer to stay anonymous.

Examples of what is in scope:

- memory safety problems, panics or unbounded resource use triggered by a
  malicious WebSocket server, scenario file or baseline file;
- TLS certificate or hostname verification being skipped when it should not be;
- credentials (tokens, API keys) leaking into logs, reports or error
  messages;
- vulnerabilities in the release artifacts, installers or Docker image.

## Responsible use

wsblast is a load generator: it opens many concurrent connections and sends
traffic as fast as it is configured to. Pointed at a system you do not
control, it can degrade or take down that service.

Only run wsblast against systems you own or have explicit, written
authorisation to test, and agree the load, schedule and target endpoints with
the system owner beforehand. Running unauthorised load tests can violate
terms of service and computer misuse laws. Reports that depend on using
wsblast against third-party systems without permission are out of scope.
