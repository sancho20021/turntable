# turntable

## Comments

The default is **no comment**. A comment earns its place only by carrying
something a competent reader cannot get from the code in front of them.

Before keeping any comment, apply these tests. Each one is a hard fail.

- **Deletion test.** Delete it and reread the code. If nothing is lost, it stays
  deleted.
- **Novelty test.** Name the fact it carries that is not in the file: a hardware
  quirk, a spec or upstream bug, a measured number, a failure mode, an
  invariant another thread depends on. No such fact, no comment.
- **No comparatives.** `rather than`, `instead of`, `not X but Y`, `used to`,
  `now`, `previously`. These narrate a diff or restate the line below. Git holds
  the history; write in the timeless present, as if the code had always read
  this way.
- **No labels.** Do not summarize or announce a block. `// load the record` over
  code that loads a record is noise.
- **No restating.** If the sentence is the line below it in English, drop it.
- **No defending the choice.** Do not explain why some other approach was not
  taken, or what the code deliberately avoids doing. That belongs in the commit
  message. Document what the code guarantees and where that guarantee stops.
  `would` is the tell: a sentence about what *would* happen is about code that
  is not there. Delete it.
- **Plain words.** Say what happens, concretely. No jargon, no clever phrasing,
  no compressed noun stacks. If a sentence needs to be reread to parse, rewrite
  it. Domain terms the code itself uses are fine; literary ones are not.

What survives: intent behind a non-obvious choice, edge cases, performance
trade-offs, cross-thread or cross-module dependencies, and constraints imposed
from outside the file. Keep them short — fragments, not paragraphs. Prefer
linking a reference (spec, Wikipedia, issue) over deriving theory inline. Use
`///` and `//!`, matching the density of the surrounding module.

Worked example, from the TUI pitch display:

```rust
// BAD - narrates the change, and the line below already says it
// Show the deviation from nominal rather than the multiplier the engine carries.

// BAD - carries the right fact, buried in jargon
// `+ 0.` folds away the negative zero a pitch a hair under nominal
// rounds to, which would otherwise print as "-0.0%".

// GOOD - same fact, said plainly
// A pitch just under 1.0 rounds to -0.0, which prints as "-0.0%".
// Adding zero turns that back into a plain 0.0.
let percent = ((pitch - 1.) * 1000.).round() / 10. + 0.;
```

`Cargo.toml` takes no comments at all. Change the dependency or feature line and
nothing else.

Measurements, before/after numbers, and test names belong in the chat reply or
the commit message, never in a comment.
