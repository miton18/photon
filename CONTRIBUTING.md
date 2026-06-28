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

## Commit messages — Conventional Commits

**All commits must follow [Conventional Commits](https://www.conventionalcommits.org).**
The format is:

```
<type>(<optional scope>): <short summary>
```

Common types: `feat`, `fix`, `docs`, `refactor`, `perf`, `test`, `chore`, `build`,
`ci`, `style`. Optional scope is the area touched (`ui`, `server`, `db`, `ml`,
`plugins`, `auth`, …). Examples:

```
feat(ui): add 3D constellation view to the People tab
fix(auth): reject TOTP code replay within the drift window
docs: warn that Photon is alpha software
refactor(db): squash migrations into a single initial schema
```

Use `!` after the type/scope (or a `BREAKING CHANGE:` footer) for breaking changes,
e.g. `feat(server)!: require DATABASE_URL to start`.

## Contributor License Agreement (required)

Photon is **source-available under the PSAL**, and the author also offers paid
**commercial licenses**. For that to work, the author needs the right to relicense
every contribution as part of the whole project. So **all contributions are covered
by the [Contributor License Agreement (`CLA.md`)](CLA.md).**

In plain terms: **you keep the copyright to your work** — you just grant the author a
broad, sublicensable license to it, including the right to include it in commercial
editions of Photon. You accept the CLA by **signing off your commits**:

```bash
git commit -s -m "fix(server): ..."
```

This adds a `Signed-off-by: Your Name <you@example.com>` line, which certifies the
[DCO](https://developercertificate.org) **and** your agreement to the CLA. PRs whose
commits aren't signed off can't be merged.

## Pull requests

- **Small, obvious fixes** (typos, clear bugs, doc corrections) — just send the PR.
- **Anything larger** — link to the issue/discussion it implements.
- Keep the change focused; match the surrounding code's style and conventions.
- Use Conventional Commit messages (above) and **sign off** every commit (`-s`).
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
