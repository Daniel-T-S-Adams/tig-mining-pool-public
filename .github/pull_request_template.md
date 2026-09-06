<!-- Body must start with "Closes #<issue>" (or "Advances #<issue>" when
criteria remain open) — every PR traces to an issue. -->

## What

<!-- What changed and why, in terms a reviewer who reads only this section
could act on. -->

## Definition of done

- [ ] Scope stayed within the issue's stated goal and non-goals
- [ ] `make check` passes with no masked failures
- [ ] Changed behavior and its important failure path have tests
- [ ] Affected design docs / fixtures / ADRs updated in this PR (or a
      follow-up issue is filed and linked)
- [ ] No secrets, credentials, or `.env`-style files added; money stays
      integer decimal strings
- [ ] AI review findings resolved or explicitly rejected with a reason below

## Evidence

<!-- Test output, live-run timelines, measurements — what a reader needs to
trust this change without re-running it. -->

## Rejected review findings (if any)

<!-- finding → reason it does not apply. Deleting this section means "none". -->
