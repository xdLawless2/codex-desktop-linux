# Contributing to Codex Desktop for Linux

Thank you for your interest in contributing! This project packages the upstream
OpenAI Codex Desktop application for Linux.

## Getting Started

1. **Fork** the repository.
2. **Clone** your fork and install dependencies:
   ```bash
   git clone https://github.com/<your-username>/codex-desktop-linux.git
   cd codex-desktop-linux
   npm ci
   ```
3. **Create a branch** for your change:
   ```bash
   git checkout -b fix/your-fix-description
   ```

## Types of Contributions

- **Bug fixes:** Packaging, installation, sandbox, and desktop integration issues.
- **Platform support:** Patches for new distributions, display servers, or architectures.
- **Computer Use:** KWin, AT-SPI, and xdg-desktop-portal improvements.
- **Documentation:** README improvements, troubleshooting guides, translations.
- **CI/CD:** Workflow improvements, build optimizations.

## Development Workflow

1. Install `7zip`, `icnsutils`, Node.js 24, npm, Rust, and the official Codex CLI.
2. Download the official upstream DMG yourself (see `scripts/update.sh` for the URL).
3. Run `npm ci` and `bash scripts/setup.sh ./Codex.dmg`.
4. Make your changes to the packaging scripts, Linux UI patch, or native helper.
5. Run the relevant source checks, build with `npm run build:linux`, and verify
   with `bash scripts/smoke-verify.sh`.

The project is unofficial and not affiliated with OpenAI. Do not commit the
upstream DMG, extracted application, proprietary binaries, or generated packages.

## Commit Messages

Use clear, descriptive commit messages:

```
fix(postinst): preserve sandbox permissions
feat(sky-wayland): expose focused element metadata
feat(ci): add arm64 build matrix
docs(readme): add troubleshooting section
```

## Pull Request Process

1. Ensure your branch is up to date with `main`.
2. Describe what changed and why.
3. Include testing steps you performed.
4. Keep PRs focused — one logical change per PR.

## Code of Conduct

Be respectful and constructive. This project follows the
[GitHub Community Guidelines](https://docs.github.com/en/site-policy/github-terms/github-community-guidelines).
