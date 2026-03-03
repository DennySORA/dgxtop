# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Check Commands

```bash
# Formatter (Required)
cargo fmt --all -- --check

# Linter (Required: Clippy)
cargo clippy --workspace --all-targets --all-features -- -D warnings

# Type Checking (Required)
cargo check --workspace --all-targets --all-features

# Tests (Required)
cargo test --workspace --all-features
```

## Development Guidelines

Follow the rules below. Decompose the system into **components**, keep each responsibility **simple and focused**, and ensure **every feature is implemented end-to-end**.

For the requirements below, **verify each item one by one**. You must **understand, validate, analyze, design, plan, implement**, and **fix all defects**. Use **sequential thinking** for planning.

## Design Standards (Mandatory)

### SOLID (Required)

- **S (SRP) — Single Responsibility:** Each module/class/function should do one thing. It should have only one reason to change.
- **O (OCP) — Open/Closed:** Add new behavior via extension (interfaces/strategies/dependency injection) and avoid modifying core logic that could introduce regressions.
- **L (LSP) — Liskov Substitution:** Subtypes must be substitutable for base types without changing the contract semantics (inputs/outputs/exceptions).
- **I (ISP) — Interface Segregation:** Prefer small, focused interfaces. Avoid "fat interfaces" that force consumers to depend on methods they do not use.
- **D (DIP) — Dependency Inversion:** High-level policy depends on abstractions. Inject IO/external systems via interfaces to enable testing and replacement.

### Clean Code (Required)

- Use specific, readable, searchable names; avoid abbreviations and vague terms (e.g., `data`, `info`, `tmp`).
- Keep functions short and single-purpose; avoid deep nesting (refactor if nesting exceeds ~2 levels).
- Design APIs around intent; call sites should read like natural language.
- Avoid duplication (DRY) but also avoid premature/over-abstraction; abstractions must reduce future change cost.
- Comments should explain **why**, not repeat **what**. If a comment explains what the code does, the code should be made clearer.

### Architecture & Code Structure

- Clear layering: **Domain (business logic) must not directly depend on Infrastructure (DB/HTTP/Queue)**. Use interfaces to isolate dependencies.
- **No business logic in Controllers/Handlers:** Handlers only handle input validation/authentication/authorization/transformation and invoke use-cases.
- Clear module boundaries: cross-module access must go through public APIs; do not rely on internal implementation details.

### Error Handling & Observability

- All errors must be **traceable**: include specific error codes/messages and required context (request id / user id / correlation id).
- Separate errors by layer: **Domain errors vs. Infrastructure errors** must not be mixed.

### Testing (Required)

- Every new/changed behavior must include tests covering at least:
  - Primary success path
  - Critical failure paths (insufficient permissions, invalid input, external dependency failures)
  - Edge cases (null/empty values, max length, time boundaries, concurrency)
- Unit tests must not depend on real external systems (DB/HTTP). Use stubs/mocks/test doubles.
- Bug fixes must include a **failing test first**, then the fix (to prevent regressions).

## Maintainability & Consistency (Required)

### Formatting & Static Analysis

- Must enable: formatter, linter, and type checking (use them wherever applicable).

## Security Standards

### Sensitive Data & Credentials

- Credentials/keys/tokens **must not be committed to source code or the repository**.

## Git Commit Guidelines

All new commits must strictly adhere to the following rules. This ensures a clean history and supports automated changelog generation.

### Core Format (Conventional Commits)

```text
<type>(<scope>): <imperative summary>
```

#### Allowed Types

- **feat**: A new feature (user-facing capability, new module/endpoint).
- **fix**: A bug fix.
- **docs**: Documentation only changes.
- **refactor**: A code change that neither fixes a bug nor adds a feature.
- **perf**: A code change that improves performance.
- **test**: Adding missing tests or correcting existing tests.
- **build**: Changes that affect the build system or external dependencies.
- **ci**: Changes to CI configuration files and scripts.
- **chore**: Other changes that do not modify src or test files.
- **style**: Changes that do not affect the meaning of the code (white-space, formatting, etc.).
- **revert**: Reverting a previous commit.

#### Scope

- Specify the top-level folder or subsystem changed (e.g., `api`, `ui`, `auth`, `deps`, `config`).
- If the change affects the entire system, the scope can be omitted, but it is preferred to include it.

#### Title Guidelines

- **Length**: Preferably 50 characters, max 72 characters.
- **Mood**: Use the **imperative mood** (e.g., "add" not "added", "fix" not "fixed").
- **Punctuation**: Do not end the title with a period.
- **Language**: Must be in **English**.

### Message Body

A detailed body is **mandatory** for all non-trivial commits.

#### Content Requirements

1. **Based on Diffs**: The description must reflect the actual file changes. Do not guess.
2. **Bulleted List**: Use hyphens (`-`) or asterisks (`*`) for formatting.
3. **Structure**:
   - **What**: detailed list of changes (files, functions, configs).
   - **Why**: context or reason for the change (if inferable).
4. **Tests**: You must include a `Tests:` line indicating how the change was verified.
5. **Dependencies**: If dependencies are changed, state the package name and the action (bump/pin/remove).

### Breaking Changes

If the commit introduces a breaking change (incompatible API change):

1. **Header**: Add an exclamation mark `!` after the type/scope (e.g., `feat(api)!: ...`).
2. **Footer**: Include a `BREAKING CHANGE:` section at the bottom of the body describing the change and migration path.

### Author Identity

- **Name**: `user.name` must be set to **DennySORA**.
- **Email**: Use your actual email address.

### Commit Examples

#### Feature (feat)

```text
feat(auth): implement JWT token refresh mechanism

- What: Added new endpoint /api/v1/refresh to handle token renewal.
- What: Updated AuthMiddleware to check for expiration before rejecting requests.
- Why: To improve user experience by preventing forced logouts when access tokens expire.
- Dependency: Bumped jsonwebtoken from 8.5.1 to 9.0.0.

Tests: Verified with Postman and ran unit tests (npm test:auth).
```

#### Bug Fix (fix)

```text
fix(ui): resolve button misalignment on mobile devices

- What: Adjusted CSS flexbox properties in ButtonComponent.vue.
- What: Removed fixed width constraints causing overflow on screens < 375px.
- Why: Users on iPhone SE were unable to click the "Submit" button.

Tests: Tested on Chrome DevTools mobile view.
```

#### Breaking Change

```text
refactor(database)!: drop legacy user columns

- What: Removed 'age' and 'gender' columns from the Users table.
- Why: These fields are no longer collected due to new privacy policy compliance.

BREAKING CHANGE: The 'age' and 'gender' fields are removed from the User schema. Any code relying on these fields will fail.

Tests: Ran db:migrate and existing user tests.
```

### Pre-commit Checklist

- [ ] The working tree is clean (no unintended files).
- [ ] The message is written in **English**.
- [ ] The title follows `type(scope): imperative summary`.
- [ ] The body includes specific "What" and "Why" bullets.
- [ ] The body includes a "Tests" line.
- [ ] If breaking, `!` and `BREAKING CHANGE:` are used.
