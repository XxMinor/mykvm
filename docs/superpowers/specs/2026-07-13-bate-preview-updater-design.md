# Bate Preview Updater Design

## Goal

Publish the current feature work through the `dev` preview branch and make preview builds update only from `aceleisureman/mykvm`'s beta release channel.

## Design

- Do not trigger releases from `main`; it remains the untouched upstream-sync branch.
- Treat pushes to `dev` as beta releases with versions such as `0.9.9-beta.<run>`.
- Build signed macOS, Windows, and Linux updater artifacts with the existing release workflow.
- Publish the preview updater manifest at `https://github.com/aceleisureman/mykvm/releases/download/beta/latest.json`.
- Configure beta build artifacts to check that manifest. Stable builds continue using this repository's latest stable release manifest.
- Move the current branch's completed changes onto `dev` without including the untracked `claude_auto_continue.py` file.

## Safety and Validation

- Preserve Tauri updater signature verification and the existing signing public key.
- Validate workflow YAML and repository URLs.
- Run Rust tests and the frontend production build before handing off.
- Do not push unless explicitly authorized; local branch preparation and commits are allowed.
