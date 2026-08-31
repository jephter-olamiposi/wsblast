# Gemini Instructions

This repository is a monorepo. For this project, Gemini must work only inside the `backend/` folder unless the user explicitly says otherwise.

Do not edit:
- `web/`
- frontend files
- rest/mobile folders
- shared UI or design files
- generated frontend assets

Use [NOTES-FOR-BACKEND.md](NOTES-FOR-BACKEND.md) as the master backend guide.

## Skill Folder

Gemini-specific reusable skills live in [.gemini/skills](.gemini/skills).

For backend work, read and follow the skills in this folder before implementation.

When making backend changes:
- keep source code, tests, migrations, config, and docs inside `backend/`
- use the Figma file only as backend product context
- translate product flows into APIs, data models, auth, permissions, validations, notifications, and admin workflows
- preserve a consistent API response, error, pagination, filtering, sorting, and versioning contract
- **Crate & Documentation Research:** Always verify dependency versions in `Cargo.toml`. You MUST look up official documentation on [crates.io](https://crates.io) or [docs.rs](https://docs.rs) for that exact version before writing code. Do not guess API shapes or use outdated syntax.
- **Avoid Common AI Pitfalls:**
  - **No N+1 SQL Queries:** Do not run queries in loops. Use joins, batches, or SQLx `ANY` bindings.
  - **Tenant/User Scoping:** Explicitly scope all data fetches to the tenant/user context unless it is a public resource.
  - **Ownership & Lifetime Compliance:** Ensure futures and closures meet thread-safety (`Send`, `Sync`) and lifetime (`'static`) bounds.
  - **Strict Error Handling:** Use custom project error types and the `?` operator. Production code must not contain `.unwrap()`, `.expect()`, `todo!`, or `panic!`.
- prefer OpenAPI/Swagger documentation for API contracts
- run relevant backend checks/tests when possible
- before finishing, confirm changed files are inside `backend/`
