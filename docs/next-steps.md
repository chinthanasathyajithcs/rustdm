# rustdm: where we are and what's next

Updated: 2026-10-13

## Done

- Direction agreed: Rust, CLI-first engine for milestone 1.
- Design spec written (draft): `docs/superpowers/specs/2026-10-13-rustdm-milestone-1-design.md`.

## Waiting on the author

1. **Review the spec.** Section 8 lists the decisions made on the author's behalf; change any of them. Approval unlocks the implementation plan.
2. **Pick the development setup** (see "Open decision" below).
3. **Dependency approvals**, asked at the moment each is needed:
   - `tokio` when the test server is built (step 3).
   - `reqwest` when the probe is built (step 4). This one does core download work, so it needs an explicit yes.
   - Small utilities (`serde`, `serde_json`, `clap`, `url`, `thiserror`) are confirmed in the plan.

## Open decision: where to build

No Rust toolchain is installed yet, and the target is Windows 11.

- **Option A, start in WSL:** quick to install; fine for the pure-logic steps (1 to 3: ranges, template, test server). Windows-only behavior (`seek_write`, reserved names, rename semantics) would be wrapped in `cfg` code and only truly exercised in CI or on Windows.
- **Option B, Windows-native toolchain:** `rustup` plus the MSVC build tools (several GB). It is the real target, and needed before step 5 (writer).
- **Recommendation:** start with A for steps 1 to 3, add B before step 5, and run Windows CI from the first commit.

## Next actions, in order

1. Author reviews the spec and answers the open decision above.
2. Done: private GitHub repo created, and the spec committed on branch `docs/milestone-1-spec`.
3. Write the implementation plan from the approved spec.
4. Install the Rust toolchain for the chosen setup.
5. Build step 1, `ranges`: explain the concept, write the tests first, then the code. Details of the 11 steps are in section 7 of the spec.

## Risks to keep in mind

- A very slow single connection on the author's network; use several parallel connections when downloading anything large.
- Antivirus may slow writes to the `.part` file; measure before tuning.
- Real servers lie about ranges and lengths; the test server exists to prove we handle it.
