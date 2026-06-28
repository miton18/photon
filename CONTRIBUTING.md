# Contributing to Photon

Thank you for caring about Photon! The project is early and **maintained by a
single author**, so the contribution process is deliberately small and clear.

## TL;DR

| You want to…              | Do this                                            |
| ------------------------- | -------------------------------------------------- |
| 🐞 Report a bug           | Open a **[GitHub issue](../../issues)**            |
| 💡 Request a feature      | Open a **[GitHub Discussion](../../discussions)**  |
| 🔧 Fix an obvious typo/bug | Send a small PR                                    |
| ✨ Build a new feature     | **Discuss it first** (see below)                   |

## 🐞 Bugs → Issues

If something is broken, [open an issue](../../issues/new/choose). A good report has:

- What you did (steps to reproduce).
- What you expected vs. what happened.
- Your environment: OS, how you run Photon (cargo / Docker), and the relevant
  component (server, ui, ML sidecar, a plugin…).
- Logs or screenshots if you have them.

## 💡 Feature requests → Discussions

**Please do not file feature requests as issues, and do not open a pull request
for a new feature out of the blue.** Start a
**[Discussion](../../discussions)** instead. That's where ideas are gathered,
debated, and prioritised against the roadmap before any code is written. It saves
everyone from work that can't be merged because it doesn't fit the project's
direction.

Once a discussion lands on "yes, let's build this", *then* it becomes an issue and
a PR is welcome.

## Pull requests

- **Small, obvious fixes** (typos, clear bugs, doc corrections) — just send the PR.
- **Anything larger** — link to the issue/discussion it implements.
- Keep the change focused; match the surrounding code's style and conventions.
- Make sure it builds and tests pass for the crate(s) you touched (see below).

## Project layout & building

Photon is a monorepo but **not a Cargo workspace** — each Rust crate is
independent and built from its own directory.

```bash
# Server (needs Postgres for the test suite)
cd server && cargo build
DATABASE_URL=postgres://photon:photon@localhost:5432/photon cargo test

# Web UI
cd ui && pnpm install && pnpm check && pnpm build

# A plugin (each is its own crate)
(cd plugins/example-hello-job && cargo build)
```

See the [README](README.md) for the full getting-started guide and architecture
overview.

## Code of conduct

Be kind and constructive. Assume good faith. This is a small project built in the
open — treat other contributors the way you'd want to be treated.
