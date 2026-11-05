# Security policy

## Supported versions

Security fixes go into the latest release. Please update before reporting a problem in an older version.

## Reporting a vulnerability

Please do not open a public issue for a security problem. Use *Report a vulnerability* in the repository's **Security** tab (GitHub private vulnerability reporting) and describe:

- what you found and which version of MATE (`mate --version`) and platform you used,
- how to reproduce it, ideally with a minimal broker setup,
- what an attacker could do with it.

You will get an answer as soon as possible; a fix is released together with a note in the [changelog](CHANGELOG.md) once it is available.

## What MATE protects

MATE never stores broker passwords or private-key contents in its TOML files (passwords live in the operating system's keyring, imported private keys get restricted file permissions) and never disables TLS certificate verification. Reports that show otherwise are in scope. Dependencies with known advisories that do not affect MATE are listed with their reasoning in [deny.toml](deny.toml).
