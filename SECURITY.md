# Security Policy

## Supported Versions

Only the latest release is actively monitored for security issues.

## Reporting a Vulnerability

If you discover a security vulnerability in this project, please report it
responsibly:

1. **Do not** open a public issue.
2. Use GitHub's private vulnerability reporting for this repository with a
   description, reproduction steps, and potential impact.

## Scope

This repository contains packaging scripts and the Linux Wayland Computer Use
helper source. It does not track the upstream DMG, extracted application,
proprietary binaries, or built Linux packages. Security concerns specific to
the Codex Desktop application itself should be directed to
[OpenAI](https://openai.com/security/).

## Known Considerations

- This is an unofficial repackaging project and is not endorsed by OpenAI.
- The repository publishes source only. Users obtain the upstream DMG and build
  packages locally.
- No unsigned APT repository or remote root installer is published.
