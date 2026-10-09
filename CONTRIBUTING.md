# Contributing to ravnori

Thanks for helping. Two things to know before you send a change.

## Licence and the contributor agreement

ravnori is licensed under the GNU Affero General Public License v3.0 (`LICENSE`), and its
authors also offer it under a commercial licence to organisations the AGPL does not
suit. That is how ravnori pays for itself, so every contribution has to be usable under
both.

By opening a pull request you agree that:

1. You wrote the contribution, or have the right to submit it, and it does not include
   code under a licence incompatible with the AGPL.
2. You license it to the project under the AGPL-3.0, and you grant the ravnori authors a
   perpetual, worldwide, royalty-free licence to use, modify, sublicense and distribute
   it under other terms too (including a commercial licence).
3. You keep the copyright in your contribution.

Write "I agree to the ravnori contributor terms in CONTRIBUTING.md" in your first pull
request. Changes without it cannot be merged.

## How changes are judged

- **Measured, not assumed.** A change to recall runs `rvn eval --gate` and passes it.
  A claim about speed or quality comes with the numbers.
- **Nothing lost.** Capture must never drop or duplicate events; `rvn doctor` must say
  so when anything is behind.
- **Tests with the change.** `cargo test --release --features fastembed` and
  `cargo clippy --all-targets` pass, and new behaviour has a test that fails without it.
- **The viewer keeps every feature.** A change to `ui/` passes the browser checks in
  `tests/viewer` (`cd tests/viewer && npm ci && sh run.sh`); a change meant to alter only
  the look also leaves their fingerprint unchanged (see `tests/viewer/README.md`).
