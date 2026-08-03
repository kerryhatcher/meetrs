# meetrs

## Git

**Commit often.** Small, self-contained commits over one big one at the end. Each commit
should leave the repo in a working state. Don't batch unrelated changes.

**Use Conventional Commits** — `<type>(<optional scope>): <description>`

Types: `feat`, `fix`, `docs`, `refactor`, `test`, `chore`, `build`, `ci`, `perf`, `style`, `revert`

Rules:
- Description in imperative mood, lowercase, no trailing period (`add x`, not `Added x.`)
- Breaking changes: `!` after the type/scope (`feat!:`) and a `BREAKING CHANGE:` footer
- Body (optional, after a blank line) explains *why*, not *what*

```
feat(recorder): add VAD-based segmentation
fix(transcribe): handle zero-sample tap output
docs(research): index prior macOS audio capture research
```

This repo is signed — commits use the personal GPG key configured in `.git/config`.
